# Evaluation Report

- **Date:** 2026-09-04
- **Binary:** current tree (`trace-cli` release)
- **Solver budget:** 800,000 pops (`TRACE_SOLVE_BUDGET_POPS`)
- **Machine (timings):** Linux, 16 logical CPUs, `--jobs 8`, minimal SQLite
  export — the per-corpus timing tables below were measured there. The
  2026-09-04 metric re-capture was run on macOS (Darwin), 8 logical CPUs,
  with the same `--jobs 8` and solver budget. The timings are not portable.
  Neither are the bulk totals *exactly*: the parallel index drifts a little
  run to run, which is why `eval_expected.json` gives function and edge counts
  a tolerance band. What is machine-independent is the set of exact metrics —
  `diagnostics`, `edges_indirect`, `dlsym_edges` and the dispatch target sets —
  and those are the numbers to compare when attributing a change. The
  C++-slice probes are *not* in that set: they are `min` and `band` thresholds,
  sized to catch a collapse rather than to pin a value.

**Re-verified 2026-09-12 (`->` through an undeclared template wrapper, #86):**
fresh release builds of `master` (769f2e8) and the branch were compared on
the same machine against the three clean pinned checkouts under
`/private/tmp/corpora`, `--jobs 8`, 800,000-pop budget. The branch index is
bit-reproducible: on each corpus the SQLite dump (minus `analysis_run`) is
byte-identical across three `--jobs 8` runs and one `--jobs 1` run. Eval
91/91 with the re-captured expectations.

| Metric | hdf `master` → #86 | hiview `master` → #86 | camera `master` → #86 |
|---|---:|---:|---:|
| Functions defined | 10,246 → 10,246 | 7,779 → 7,783 | 19,016 → 19,020 |
| Functions external | 2,392 → 2,378 | 3,623 → 3,191 | 6,373 → 5,353 |
| Direct edges | 42,212 → 42,240 | 8,266 → 8,988 | 21,059 → 37,646 |
| Indirect edges | 4,642 → 4,642 | 24 → 24 | 109 → 108 |
| External edges | 28,826 → 28,794 | 20,329 → 20,003 | 52,214 → 53,177 |
| Arg-flow edges | 65,961 → 65,986 | 9,585 → 9,973 | 17,104 → 24,482 |
| Diagnostics | 1,803 → 1,803 | 2,989 → 2,989 | 4,860 → 4,860 |

An undeclared template spelling keeps its arguments in its tag now
(`OHOS::CameraStandard::sptr<OHOS::CameraStandard::CaptureSession>`), and
`->` on such a value with exactly one argument that names a declared class
looks the member up on that class, with the usual hierarchy fan-out. A
wrapper whose body is in the tree still resolves through its own
`operator->` — a forward declaration alone says nothing about one and does
not count — `.` still belongs to the wrapper, and every other shape (two
arguments, a scalar, pointer or reference argument, an unknown class)
stays unresolved rather than inventing a member on the wrapper. The
argument is looked up the way C++ looks a name up: through the enclosing
namespaces innermost first, then the global scope, for a bare and a
partially qualified spelling alike (`sptr<CameraStandard::CameraInput>`
inside `namespace OHOS`), a typedef standing for the class it names,
`T const` read as `T`. Not through `using namespace` directives: hdf's
hc-gen defines `AstObject::IsNode` under `using namespace OHOS::Hardware`
and the indexer names that body with the bare class, so searching the
directive found the real `OHOS::Hardware::AstObject`, whose members are
prototypes there, and 189 hdf direct edges turned external. Only the bare
spelling reaches those bodies; the directive stays out of the search.

Camera is where it lands. Comparing distinct edges by (file, line, column,
caller, callee): **17,739** direct edges gained (13,528 in test and fuzzer
files, 4,211 in production code; the largest targets are
`CameraInput::Open` 842, `CameraInput::GetCameraDevice` 769,
`CaptureSession::AddOutput` 531, `CaptureSession::BeginConfig` 317) and
**1,152** lost at 808 sites, 777 of which carry a replacement edge. Most
lost edges were wrong: an unqualified `sptr<T>` used to lower to an
*unknown* type, so `cameraInput_->Open()` bound by bare name to whatever
free `Open` / `Release` / `SetCallback` / `Stop` a fuzzer happened to
define (`test/fuzztest/hcapture_session_fuzzer/hcapture_session_fuzzer.cpp:272-282`);
`frameworks/cj/camera/src/metadata_output_impl.cpp:121`
(`metadataOutput_->SetCallback(...)`) reaches `MetadataOutput::SetCallback`
now. The 31 sites left without an edge call through a wrapper whose
argument class is not in the tree — `recorder_->Start()` on a
media-library recorder in
`frameworks/native/camera/base/src/output/movie_file_output.cpp:123` —
where `master` had one of those fuzzer functions. The one indirect edge
lost is the same fault: `avcodec_task_manager.cpp:124` called
`->GetTimeStamp()` on a `sptr<FrameRecord>` and reached a fuzzer lambda
through a free function; it is a direct `FrameRecord::GetTimeStamp` now.
The **254** `OHOS::sptr::*` arrow phantoms are gone and a probe pins them
at 0; the constructor and `.GetRefPtr()` are the wrapper's own operations
and stay, under whichever namespace the spelling qualifies to. Defined
functions rise by 4 because a tag keeps its arguments, so overloads that
differ only inside them no longer fold: hiview's
`ParamValue(const std::vector<uint32_t>&)` beside
`ParamValue(const std::vector<std::string>&)`
(`base/event_report/event/param_value.cpp:47,52`).

*A second fault, found while addressing review.* The reviewer asked for
the enclosing-namespace lookup above, and with it camera lost 85 correct
direct edges — `photoAssetIntf_->AddPhotoProxy(...)` in
`common/utils/photo_asset_proxy.cpp:101` stopped fanning out to
`PhotoAssetProxy` / `PhotoAssetAdapter` and went to a bare
`PhotoAssetIntf::AddPhotoProxy` prototype. `PhotoAssetIntf` is declared in
`common/utils/photo_asset_interface.h` inside a C++17
`namespace OHOS::CameraStandard {`, which `lower_namespace` read as an
*anonymous* namespace: tree-sitter spells the name as one
`nested_namespace_specifier`, the lowering looked only for a
`namespace_identifier`, and everything inside registered under the bare
name with internal linkage. The old innermost-only qualification happened
to match the properly qualified declaration from other units; the correct
lookup found the mis-registered bare class first. 83 hiview files and 62
camera files open a namespace that way (hdf none). Fixed: the specifier's
segments push one scope each. Every direct edge that changes from this
(hiview 146, camera 427) has the same call under the proper qualification,
diagnostics do not move, and hdf is byte-identical; hiview's external
functions fall **3,620 → 3,191** as bare-named prototypes fold into their
qualified definitions. hiview's three pinned dispatch sites
(`Plugin::OnEventProxy:68`, `PluginProxy::OnEvent:28`,
`EventHandler::OnEventProxy:232`) each gain one target,
`TraceTestPlugin::OnEvent` — a test plugin declared in such a file, which
now joins the `Plugin` hierarchy under its proper name — and the pins move
23 → 24, 23 → 24, 27 → 28. Camera's unresolved-template-text probe moves
1,246 → 1,157 because 105 sites lost their template text when a wrapper
member's spelling was stripped of its arguments (`sptr<X>::MakeSptr` is
`sptr::MakeSptr`); the resolved template-text sites fall 56 → 40 the same
way and none gains an edge.

hdf and hiview were expected not to move for #86 itself — their wrappers
are declared in-tree or on the standard name list — and they do, inside
every band and with every exact metric unchanged. hdf's remaining
`OHOS::sptr::*` phantoms (`sptr<IRemoteObject>` under `using OHOS::sptr`)
and its `SharedMemQueueMeta<T>::*` ones become direct edges to
`ServStatListenerStub::OnRemoteRequest` and
`OHOS::HDI::Base::SharedMemQueueMeta::Get*`; no other hdf row moves.
hiview loses its nine `OHOS::sptr::*` phantoms; `.` calls on unqualified
`vector<T>` / `stack<T>` / `list<T>` locals under `using namespace std`
(`utility/common_utils/log_parse.cpp:92`), which used to lower to an
unknown type and carry no edge, are external members on the container now
— exactly what their `std::`-qualified spellings have always produced.

*Performance.* A tag per instantiation raises the type-table work of every
header merge: camera's merged table grows from 5,408 to 7,281 entries, and
the first build of the branch cost ~7% more user CPU on camera. Two
output-neutral changes pay for it. `TypeTable::intern` answered an empty
named tag by cloning the richest layout under that name into it and then
looking the clone up, when the tag map already holds the id it lands on;
it answers from the tag map now, on the by-value and by-reference paths
(`empty_tag_interns_to_the_richest_layout_without_a_rewrite` pins the
equivalence, including a layout widened in place by a union). And a unit
merge remapped type ids through an `FxHashMap` keyed by an id that is a
dense index into the source table; it is a `Vec` now. Both leave all three
dumps byte-identical. Five interleaved runs of each binary, `--jobs 8`,
`bash time`:

| Corpus | `master` wall / user | #86 wall / user |
|---|---:|---:|
| hdf | 4.11s / 9.45s | 4.01s / 9.23s |
| hiview | 1.67s / 4.38s | 1.65s / 4.33s |
| camera | 6.53s / 18.08s | 6.66s / 18.33s |

hdf and hiview are faster; camera is within 2% of `master` on both axes,
which is the run-to-run spread of `master` itself on this machine (its
user time ranged 17.4s to 18.1s across the day's sessions), with 16,587
more direct edges and 7,378 more arg-flow edges to solve and export.

**Review round.** Four review findings were confirmed on probes and fixed
without moving any pinned metric outside its band: the wrapper's own name
now goes through the enclosing-scope lookup its arguments use (camera's
`BlockingQueue<std::any>` inside `DeferredProcessing` binds to the
`CameraStandard::BlockingQueue` its header includes rather than a same-named
class the header never saw, 12 direct edges → 11 as a doubled `Push`
overload edge becomes one); a typedef is matched by its whole spelling; an
out-of-line `operator->` records its return without the class header; and
`sp->f` steps through a wrapper to the pointee's field (hiview +83 field
temporaries and +121 flow edges, camera +8 and +12, call edges unchanged by
name). Output is bit-identical across three `--jobs 8` runs and one
`--jobs 1`; wall time is level with the previous push (+0.6 %, user +0.9 %,
inside the run-to-run spread).

**Second review round.** Seven further findings were confirmed on probes
and fixed. The only one that moves a corpus number is field access through
a wrapper: a field step now follows its own operator, so `sp->f` on the
wrapper variable itself reaches the pointee (it previously only did so
one step down a field chain, `b.item->f`) and `w.f` stays on the wrapper
(it previously stepped through a wrapper with a declared `operator->`
and lost the wrapper's own field). Against the previous push that adds
field steps on direct reads through standard smart pointers — hdf +34
sites (hc-gen's `forwardWalkObj->child_`), hiview +124
(`event->eventName_`), camera +985 (`userInfo->scalingFactor`) — with no
site lost anywhere; call edges are unchanged by name except one external
constructor stub that camera spelled `::OHOS::sptr::sptr` and now
`OHOS::sptr::sptr` (the `::W<T>` head fix; functions_total 24,373 →
24,372), and camera gains 7 arg-flow edges where a field read through a
wrapper now feeds a constructor argument (24,482 → 24,489). The other
fixes — a member type of a template keeping its `::` and its own name
(`Outer<A>::Inner<B>`), a member type of a defined template keeping the
class the lookup found (`CS::Defined::Iterator`), `T*const` in the cv
helper, and `nullptr_t` / `intmax_t` / `uintmax_t` / `auto` never
qualified — change no corpus number. Output is bit-identical across
three `--jobs 8` runs and one `--jobs 1`; paired against the previous
push on camera (8 alternating runs) wall is −1.2 % and user −1.5 %,
inside the spread.

**Third review round.** Three more findings were confirmed on the fixture
and fixed, none of which moves a corpus number: a literal template
argument inside a namespace was qualified like a class (`missing<true>`
in `namespace N` interned `N::missing<N::true>`; literals are now spelled
as written), a `::`-prefixed head at a type declaration was not stripped
the way the spelling helper strips it (`::Outer::Defined<int>` missed the
defined class and its layout, so a field read through it produced
nothing; the leading `::` is now handled in one place), and a pointer
level between qualifiers was dropped (`T * const *` read as `T*`). Two
small hardenings went in with them: a class is never recorded as its own
base, and merging declared-class sets is one probe per name instead of
two. Output is identical to the previous push on all three corpora and
bit-identical across three `--jobs 8` runs and one `--jobs 1`; paired
against the previous push on camera (8 alternating runs) wall is +0.6 %
and user +1.0 %, inside the spread (the two runs' ranges overlap).

**Regression investigation, 2026-09-10 (#83):**

*A C caller no longer reached its `extern "C"` C++ implementation.* Reported against
`drivers_hdf_core`'s `hdf_remote_service.c:68`, which called
`HdfRemoteAdapterCheckInterfaceToken` — defined at `hdf_remote_adapter.cpp:469` — and got an
`external` edge to the *prototype* in `hdf_remote_adapter_if.h:46` instead. Nine of the twelve
names that header declares were in that state, and the three that were not are exactly the three
with no `struct` pointer among their parameters.

The prototype and the definition reach the symbol table from two different units. The C++ overload
check compared their parameter types by `TypeId` and read the mismatch as an overload, so the two
records stayed apart and every caller kept the undefined one. The ids differed because the same C
type had been interned twice: `hdf_remote_adapter.cpp`'s unit carried
`Ptr(Struct HdfRemoteService { object: Struct HdfObject { } , ... })` while every other unit
carried the same type with `HdfObject`'s `objectId` field present. `TypeDesc` is structural and a
named aggregate's stored `desc` is a snapshot — `complete_nested_tags` repairs a type's
`layout.fields` but not its `desc`, and `canonicalize_desc` rewrites only a *bare* empty tag, not
an empty tag nested inside a non-empty one — so the order in which the PCH preamble merged
`hdf_object.h` and `hdf_remote_service.h` decided which snapshot a unit got.

Resolved parameter pairs now compare by shape. That comparison already existed for this exact
reason on the variant merge path (`same_param_type`, "two configurations of one source intern their
own copy of a type, so the same C++ parameter can arrive under different ids and an id comparison
would split a function from itself"); it moved to `trace_ir::same_param_type`, and `merge.rs` and
`symbol.rs` now share the one copy. A pair of *definitions* still compares by id: one qualified
name legitimately carries two bodies, and merging those lets the second overwrite the survivor's
span and parameters — the hazard `base_definitions` already guards. Camera
proved that necessary — the unnarrowed comparison collapsed 23 defined overload groups, folding
`IStreamOperatorMock::Capture` from two unrelated fuzzer headers together and merging
`DeferredVideoProcessingSessionCallback::OnError`'s test mock into the production implementation.

Comparing by shape was necessary but not sufficient. Four more faults each faked a mismatch
between a prototype and its definition, and each was found by asking what the residue the probes
still reported was made of. The candidate search consulted only `fn_by_name`, a single entry per
name, so a definition missed its own declaration whenever another overload had been registered
after it; it now scans the undefined prototypes in `externals_by_name` and only then falls back to
`fn_by_name` for exact duplicate-definition dedup, and `fn_by_name` itself prefers a defined
function so a later declaration cannot shadow a body. Lowering discarded tree-sitter's
`optional_parameter_declaration` nodes, so a parameter carrying a default argument vanished from
the prototype and the arity check rejected the definition it belonged to. A self-typedef
(`typedef struct Foo Foo;`) went unregistered, so `Camera_CaptureSession **` lowered as `Int`
rather than a struct pointer. And a leading `::` made `::Foo` and `Foo` distinct shapes.

These are what move `hiviewdfx_hiview`, which the shape comparison alone did not move at all: 34
declaration-only entities fold into their definitions and 62 call sites turn direct. Across the
three corpora the count of names carrying both a definition and a declaration-only entity falls
from 10 / 40 / 159 to 0 / 17 / 40; the residue is real overload sets where one member is declared
and another defined, which is not this fault. Retaining default parameters also *raises* camera's
`functions_defined` by exactly one, which is a recovery and not a duplicate: three unrelated
headers under `mediastream/test/unittest/filter` each define their own `MockNextFilter`, two taking
`(std::string name = ..., CFilterType type = ...)` and one taking `()`, and dropping the defaults
lowered the two-parameter constructors as nullary, so two of the three bodies collapsed into one
entry.

The duplicate `TypeId` itself is left in place. It is real — HDF interns 239 named tags (anonymous
ones excluded) under more than one id, `struct HdfDeviceNode` alone under 15 — and it costs
precision beyond this bug, because `FieldSummary` and points-to key on `TypeId`.

Converging it by canonicalizing an aggregate's field descriptors recursively at intern time (with a
cycle guard, so `struct Node { struct Node *next; }` keeps its self-reference as a tag reference
instead of nesting a copy per intern) **was implemented and measured, then rejected**. It works —
HDF's type table drops from 4,202 to 3,938 entries, its multiply-interned named tags from 239 to
121, and `HdfDeviceNode` from 15 ids to 2 — but the walk runs on every intern and the rewritten
descriptors are far larger:

| | index | peak RSS | `edges_indirect` |
|---|---|---|---|
| shallow canonicalization (kept) | 6.0-6.2s | ~610 MB | 4642 |
| recursive canonicalization (rejected) | 17.1s | 1.88 GB | 4666 |

Both rows are HDF on one build, taken before the lookup and lowering fixes above brought the index
to 5.6-5.8s; the comparison between them is internal to that build.

Tripling index time and peak memory is the wrong trade in a change that exists partly to answer a
performance regression, and the move in `edges_indirect` — an exact metric — needs attributing on
its own rather than riding along here. `canonicalize_desc` carries a comment recording that its
shallowness is deliberate.

A cheaper canonical form is the more promising direction and was not tried: normalize a nested
named tag *down* to its empty tag-reference form rather than up to its full layout, so a parent's
descriptor never carries a nested field list at all. That converges both orderings, shrinks
descriptors instead of growing them, and leaves field resolution to `layout.fields`, which already
interns through the tag registry — but it has to intern a nested aggregate before stripping it, or
a tag first seen inside its parent loses its fields, so it needs `canonicalize_desc` to take
`&mut self` and is a change to the type table's canonical form rather than a fix.

*Indexing had two separate regressions.* All figures below: macOS, 8 logical CPUs, `--jobs 8`,
warm filesystem cache, release build, idle machine; three runs on HDF, two on camera, reported as
the range. The ranges are from an idle machine and are the useful figures for comparing rows;
under load every row inflates together (a loaded run measured master at 12.7s and this change at
7.1s), so compare rows only within one sitting.

| commit | HDF index | HDF preprocess | camera index | camera preprocess |
|---|---|---|---|---|
| `82c1f11`, before #55 | 3.4-3.7s | — (no such phase) | 8.4-9.0s | — |
| `6615690`, #55 itself | 6.2-6.3s | 2.8s | 19.4s | 7.8s |
| `ed5b6eb`, before #62 | 6.0-6.1s | 2.7-2.8s | 18.4-19.2s | 7.5-7.8s |
| `7fee472`, master | 8.8-9.2s | 5.0-5.1s | 24.0-24.9s | 11.2-11.4s |
| this change, before the probe memo | 5.6-5.9s | 2.6-2.7s | 16.8-17.6s | 6.6-6.9s |
| this change | 4.2-4.4s | 1.3-1.4s | 10.7-10.8s | 2.4s |
| this change, second round | 2.7-2.8s | 1.0s | 5.6-5.8s | 1.5s |

The last two rows were re-measured against each other in one later sitting, which put the
pre-memo row at 5.9-6.1s / 2.7s (HDF) and 17.9-18.0s / 7.0s (camera) — within the ranges above.

`6615690` is measured because it isolates the first regression: #55 alone takes HDF from 3.6s to
6.2s and camera from 9.0s to 19.4s, and `ed5b6eb` matches it on both corpora, so the PRs merged
between #55 and #62 contribute nothing measurable. The two regressions are therefore #55's
(+2.6s HDF, +10.4s camera) and #62's (+3.0s HDF, +5.7s camera), and only the second is fixed here.

**The second regression is fixed here.** Merging the compilation database (#62) cost HDF 2.3s of
preprocessing and camera 3.7s — on trees with no `compile_commands.json` at all, producing
byte-identical output. A 4-second sample of the HDF index attributed 338 of 1338 `process_tokens`
samples to one call chain: `resolve_include` → `include_exists` →
`trace_ir::canonicalize` → `std::fs::canonicalize` → `realpath` → `__getattrlist`. #62 added a
source-cache probe under the canonical key so a virtual header can be found, but `resolve_include`
builds a fresh includer-relative candidate for every (including file, spelling) pair, so the memo
inside `trace_ir::canonicalize` almost never hits and each miss paid a full `realpath`.

The probe now uses the path as handed in. Reaching it means `is_file` said no, and
`std::fs::canonicalize` requires every component to exist, so there it can only fail and fall back
to that same path — the one input that survives it is an existing *directory*, which is never a
source-cache key. #62's own `review_virtual_headers_in_all_search_classes` still passes in all five
search classes, and with this one hunk applied on its own all three corpora produce output
byte-identical to `7fee472`.

**The first regression is mostly paid off, without the redesign it seemed to need.** `82c1f11` has
no preprocessing phase at all: making a translation unit's macro context reach its headers (#55)
introduced the two-pass scheme, and `preprocess-done` reports the two passes separately, so the
serial discovery pass is attributable on its own — 2.2s of HDF's 2.7s and 5.5s of camera's 7.0s.
That pass stays serial: it is the one pass that writes the shared expansion cache while reading
it, and running it in parallel moved camera across 20,299 / 20,326 / 20,847 direct edges on three
runs of one tree. Decoupling expansion-variant discovery from TU text generation, so a cached
expansion depends only on header content, language and macro fingerprint rather than on incidental
includer state, would let it run in parallel; **that redesign was still not attempted.** What the
three changes below do instead is make the serial pass itself cheap — 1.1s on HDF and 1.8s on
camera — which left HDF about 1.2x its pre-#55 index time and camera about 1.2x; the second round
below takes both under it (HDF about 0.8x, camera about 0.65x) with the pass still serial.

A 7-second sample of camera's index attributed **3338 of 5603 main-thread samples to `stat`**,
under `resolve_include` → `include_exists` → `Path::is_file`. The candidate a probe is built from
is `including_file.parent().join(spelling)`, so it repeats for every include of the same header
from the same directory and again for every translation unit that re-expands it, while the tree
under analysis is read-only for the whole run. Counting the probes: camera issues **4,343,229 of
them naming 224,945 distinct paths**, HDF 1,445,203 naming 135,227, hiview 1,546,148 naming
136,908 — 91-95% repeats. `trace_ir::is_file_cached` memoizes the answer per thread, keyed on the
path's encoded bytes, because `Path`'s own `Hash` walks components and normalizes separators as it
goes and on these paths costs more than hashing them flat. Camera's discovery pass: 5.5s to 2.1s.

Re-sampling then put hashing on top, ~31% of what was left, all of it SipHash over path keys. The
preprocessor's maps (and `IncludeGraph`'s source cache and basename index, and the shared
include-expansion cache) now use `rustc-hash`. This is safe for output for a reason worth stating:
`std`'s `RandomState` is seeded per process, so any output that depended on a hash-map iteration
order could not have been bit-reproducible run to run in the first place — and it is, at every job
count, which is what makes the swap a pure cost change. Camera's discovery pass: 2.1s to 1.8s
including the change below.

The third change is the one probe the memo could not answer. `include_exists` falls through to the
source cache for a candidate the filesystem rejected, and a cache built by *reading files off
disk* — which is what `IncludeGraph` hands the indexer — can never rescue one, so the fall-through
hashed a full path per failed include for nothing: 367 of 2150 samples, 17% of the pass.
`PreprocessOptions::with_source_cache` now derives whether any key names no file
(`virtual_headers`, one memoized `stat` per key at install time, and the options are built twice
per run) and the probe stops at the memoized `stat` when none does. A caller that seeds a header
the filesystem does not have keeps the conservative answer either way — through the builder,
because the scan finds its key; through a direct field assignment, because the flag defaults to
`true`. `with_source_cache_keeps_probing_for_a_header_that_is_only_in_the_cache` is the guard, and
it fails (`virtual.h` unresolved) if the derivation is forced to `false`.

What is left in the pass, from a final 2-second sample (~1450 main-thread samples): `stat` 182,
the 225k first probes that have to happen; `MacroFingerprint::signature` 106, which builds a
`DefaultHasher` per macro entry and XORs the results so insertion order drops out — left alone
deliberately, since XOR-combining a weaker per-entry hash trades a collision for replaying the
wrong cached expansion; hide-set `Arc<str>` hashing ~160, which needs macro names interned to
indices to improve; the lexer 78; and allocator traffic ~250.

Also taken from the performance list, and separate from the `realpath` fix: `LineMap::truncate_at`
cuts at a `partition_point` rather than walking every entry; the symbol table's redeclaration merge
reaches an entry through its O(1) `fn_slots` slot; the cross-TU merge indexes a unit's parameter
types once instead of scanning `unit.variables` per parameter; the include-order cycle tail uses a
set rather than an O(V^2) `Vec::contains`; and `deps::resolve_include` probes candidates lazily
instead of allocating one `PathBuf` per search directory (205-291 of them) per include. These
remove real quadratic and linear paths but are not what moves the numbers above. A lexer
token-vector `with_capacity(len / 4)` was tried and reverted for showing no benefit; peak RSS on
camera varies by more than 200 MB run to run, too noisy to attribute a memory effect to any of
these in either direction.

All three corpora are bit-identical across three runs at `--jobs 8` and at `--jobs 1` with these
changes, and every exact metric is unchanged against parent `7fee472` measured on the same machine.
See `scripts/eval_expected.json`'s note for the re-captured bulk totals, including one correction
that predates this change: hdf's `functions_defined` had been pinned at 10223. That was a real
measurement as of `6615690`, where it summed with that revision's 2332 external to exactly the
12555 total pinned beside it — it simply went un-refreshed when later PRs moved hdf's total and
external pins to 12649 and 2403, leaving 10223 + 2403 = 12626, 23 short of the total it sat next
to. The parent measures 10246, and this change leaves it there.

**Second performance round, 2026-09-11.** Two costs the samples above had hidden, and four
smaller ones. The macOS sampler attaches late enough to miss a phase that finishes in the first
half second at `--jobs 8`, which is how the include-graph build stayed invisible: a `--jobs 1`
sample put 2.3s of `stat` under `IncludeGraph::build_with_deps`, and `time` showed the same
figure another way — 10.4s of system time on camera at *every* job count against 17s of user
time. `deps::resolve_include` probed each candidate with its own `Path::is_file`, so an include
naming no project file (`<string>`, `<vector>`) walked all 205-291 search directories, once per
file that spelled it. It now goes through `is_file_cached`, and a first probe is answered from the
parent directory's listing when that settles it: a name the listing does not hold is a miss without
a `stat`, while a listed, case-folded or non-ASCII name still asks the filesystem, so a case-folding
or normalizing filesystem answers exactly as before. Camera's system time: 10.4s to 1.3s. The
other hidden cost was serial: `finalize_extern_callees` rescanned every call site once per
synthesized name, quadratic on camera at 1.5s of the index, visible only in the main thread's
subtree of the profile rather than at the top of any stack; the sites each name may claim are now
gathered once. The smaller four: `TypeTable::intern` asks the table before cloning or re-boxing a
descriptor whose canonical form cannot differ, and merging interns by reference, since a header's
types are re-interned per unit into a table that usually has them; the lowering path's path-keyed
maps, the PAG's index maps and the solver's `IndexSet<LocId>` (30% of HDF's analysis phase) hash
with `rustc-hash`; the directory walk behind `#include` is shared across preprocessing runs under
the same search lists; and the parallel index phases merge one batch while the pool parses the
next, so the serial merge (0.35s on camera) overlaps parsing. What remains serial on camera: the
warm pass (0.9s), the discovery pass (1.3s, #55), and export (0.6s). Wall on `--jobs 8`: camera
9.9-10.3s to 6.1-6.4s, HDF 5.5-6.2s to 4.0-4.7s, hiview 2.9s to 1.7s. Every step was checked the
same way: DB dumps byte-identical across three `--jobs 8` runs, one `--jobs 1` run and the previous
binary on all three corpora, eval 90/90.

**Review passes on this change (#83), 2026-09-10:**

Four review rounds ran against the branch. Findings were triaged by reproducing each one rather
than by reading the patch, and four of them turned out to be defects this change had *introduced*
in `same_param_type` — a function master does not have at all, since master compared parameter
types by exact `TypeId`. Loosening that comparison is the core of the fix, and each fault below is
a way the loosened version was too loose.

The fourth round's finding is the sharpest of them, because the wildcard it names was *inherited*
rather than written: `same_type_shape` came out of `trace-parse`'s variant merge, where
`TypeDesc::Unknown` matching anything is correct, and moving it into `trace-ir` for
`register_function` carried the wildcard across without the guard that made it safe. The variant
merge takes a candidate only when exactly one matches — `matching.next().filter(|_|
matching.next().is_none())`, with the comment "Unknown types can match several overloads. Keep the
ordinary merge path in that case" — while `register_function` takes the first compatible candidate
out of an overload bucket. So a C++ prototype whose parameter type no header in the include path
declares absorbed `f(int)`, and `f(double)`'s body then landed on the same entry, leaving callers
of one overload resolved into another's body. Exact-`TypeId` comparison could not do this, since
`Unknown` has a single prelude id. `same_param_type` now treats an unresolvable type as a type at
every depth, and `same_param_type_or_unresolved` is what the variant merge asks for; a comment at
each site says why only one of them may have the wildcard.
`an_unresolved_parameter_type_is_not_a_wildcard_across_overloads` fails on the shared-wildcard
version at its first assertion. All three corpora are byte-identical across the fix, so this was
latent on the eval trees — which is also why it needed a unit test rather than a probe.

Two smaller findings from the same round, both latent for the same reason — an in-tree caller
happens to rule them out. Replacing `index_order`'s O(V^2) `order.contains` with a set lost the
growth the linear scan had (`contains` was re-evaluated as `order` grew), so a duplicate cyclic
leftover would be appended twice; every caller in-tree dedups `files` first, but the function is
`pub`, and the set now grows as it filters. And `virtual_headers`, the flag the performance work
above put on `PreprocessOptions`, was a `bool` sitting beside a separately assignable
`source_cache` field: correct at every call site today, and silently wrong — `include file not
found`, with no hint the cache was skipped — for any future one that assigns the field after the
flag was derived for a different cache. The answer now lives *inside* `trace_preproc::SourceCache`,
computed from the very map it describes on the first candidate that reaches it, so the two cannot
fall out of step and the options carry no invariant to maintain.

*An anonymous tag was recognised by an empty name, which no lowered type has.* Tags compare by
name, so the first cut added a structural comparison for anonymous aggregates, guarded on the name
being empty. `lower_tag` names an unnamed struct or union `anon_<n>` from a counter each unit seeds
from the program's, and `extract_tag_name` falls back to a bare `anon`, so the guard never fired
and the name comparison it replaced stayed in force: two units both numbering their first anonymous
tag `anon_1` had unrelated types compare *equal*, and one shared tag numbered differently by two
units compared unequal. Anonymity is decided by the prefix now, as `lower.rs` already reads it. The
test that was supposed to cover this built its types with `String::new()` — a spelling lowering
never emits — so it passed against the dead guard; it now also asserts on the real names.

*A nested array's extent was ignored.* `int (*)[10]` and `int (*)[20]` compared equal, collapsing
distinct overloads. Stated bounds must agree now, an unstated one still matches any, and the
top-level decay was extended to array-against-array so that a parameter's own discarded bound —
`int a[10]` against `int a[20]` — still reads as one function rather than newly splitting.

*The reported call path had no `cargo test` coverage.* A `.c` caller reaching a `.cpp` definition
through one `extern "C"` prototype whose parameter tag is complete in the C unit and opaque in the
C++ one was covered only by the corpora and by unit-level mocks in `merge.rs`.
`c_caller_reaches_a_cpp_extern_c_definition_across_units` builds that tree end to end; forced back
onto exact-`TypeId` comparison it reports `[(FnId(0), false), (FnId(2), true)]` — the split
prototype and body of the original report.

`LineMap::truncate_at` also stopped narrowing its `usize` argument to `u32` before comparing,
matching `slice_from`.

Two findings were reproduced and rejected. Passing the *merged* entry's definedness to
`should_take_primary` on the redeclaration path, rather than the incoming registration's, is
reachable — with two defined overloads under one name, a declaration matching the one that does
not hold the primary slot moves the slot onto it — and it is wrong: a declaration asserts no body,
so letting it reroute every unqualified call site would make resolution depend on declaration
placement and on unit merge order, which the bit-reproducibility requirement forbids.
`a_declaration_does_not_move_the_primary_slot_between_two_bodies` pins that, and fails under the
proposed version. Normalizing `.`/`..` before the source-cache probe in `include_exists` is
unreachable: the probe runs only after `is_file` says no, and every `source_cache` key is a file
`IncludeGraph` read off disk, so a cached header is always an on-disk one. Both are recorded as
comments where a reader would next raise them.

Carried forward unfixed, both pre-existing on `7fee472` and both verified there: the nullary C++
overload collapse (`void foo();` beside `void foo(int)` folds into one entry, because the
empty-parameter wildcard cannot tell a prototype *declared* empty from one whose parameters did not
lower — tightening it to exact arity when both sides are C++ fails 17 tests), and the duplicate
external definition that overwrites the survivor's span and parameters (guarding it on
`func.is_defined && !existing.is_defined` costs hdf `edges_indirect` 4642 → 4556, `arg_flow_edges`
199 rows, and every target at two function-pointer dispatch sites, so last-definition-wins is
load-bearing for hdf's dispatch resolution and its correct fix is not a guard).

A third round raised the cost of the widened candidate search itself. A pure-C definition and a
C++-parsed prototype are matched on arity alone — a `.c` body's parameter types are decayed and
would not compare equal to the header's — and that tolerance was safe only while the search
consulted the single `fn_by_name` slot. Scanning the whole `externals_by_name` bucket made arity
ambiguous: two same-arity C++ prototypes under one name, and the definition merged into whichever
had registered first. Candidates are now tried in two passes, the first demanding the signature
agree too, so the change decides only which arity-compatible candidate is taken and never whether
any is. The same round found the internal-linkage merge reporting `adopted_params` without moving
`param_type_ids` — latent, since nothing matches `static` entries by signature, but the two
branches encoded different cache semantics — and a doc comment in `merge_unit` whose new paragraph
had been inserted into the middle of a pre-existing sentence.

A fourth round found three more faults in the loosened comparison, two of them in the anonymity
repair itself. Requiring only the `anon_` prefix claimed ordinary tags — `anon_vma`, `anon_inode`
and anything else spelled that way — and since anonymous tags compare structurally, any two of
them sharing one field became the same type, which is the inverse of the fault the predicate
exists to fix; the digits are required now. The test is also applied to the leaf, because a C++
anonymous class is registered qualified (`lower_tag` calls `ctx.qualify`, so an anonymous tag in
`namespace ns` is `ns::anon_1`) and the whole-name test called that named, leaving two units that
numbered one tag differently unable ever to match. The test meant to cover this used `anonymizer`,
which does not carry the prefix at all, so it never exercised the predicate. Separately, an array
parameter inside a function-pointer type did not decay: a parameter list decays wherever the
language spells one, so `void (*)(int a[])` and `void (*)(int *a)` are one type, and the `FnPtr`
arm was comparing its parameters with the plain shape rule.

The same round established that the nested-extent check added in the previous round **cannot fire
on lowered input**. `walk_declarator_shape` is the only place that constructs a `TypeDesc::Array`
and it hardcodes `size: None`, so every lowered array is unbounded and `same_array_extent(None,
None)` is always true; `size: Some(_)` appears nowhere outside this crate's own tests. The check is
kept because the descriptor admits a bound and comparing it is the correct rule, but it guards a
contract rather than a reachable fault — worth stating plainly, since the previous round described
it as fixing collapsed overloads.

Two pre-existing lowering faults were reproduced in this round, and they were handled differently
because their measured cost differs. `typedef struct Session *SessionPtr` registered the alias as
the bare tag without walking the declarator, so every `SessionPtr s` lowered as a struct *value*
and `s->fd` decomposed against a non-pointer; `7fee472` does the same. It is fixed here: the form
appears nowhere in the three corpora (zero matches for a one-statement `typedef struct T *A;`), all
three stay byte-identical, and the fix carries its own test.

The other was left out. An unnamed parameter — `void take(S *)`, the ordinary spelling of a
prototype — reaches `walk_declarator_shape` as an *abstract* declarator, a kind it has no arm for,
so the modifiers are dropped and the parameter takes the bare type. `S` against the definition's
`S *` reads as a different signature, so the prototype splits from its body: the #83 symptom
reachable from plain C++ with no tag divergence at all, and `7fee472` splits it identically.
Supplying the three abstract arms fixes the repro (`take` becomes one defined entry, both
parameters `S *`) and passes 90 of 90 with the workspace suites green, but it renumbers the type
table on two corpora — `.dump` diffs of 669k and 1.36M lines — while moving the #83 residue probes
not at all (hdf 0, hiview 17, camera 40, all unchanged) and moving entity counts by ones and twos
in directions this investigation could not account for (hiview `functions_total` and
`functions_defined` each −2; camera total +1, defined −1, external +2, overload groups −1). A
change with no measurable effect on the reported fault, an unexplained effect on entity counts and
a whole-corpus renumbering behind it needs its own attribution and its own expectation re-capture,
not a ride on a branch whose case rests on being byte-identical. The same applies to
`is_parameter_node` not matching `variadic_parameter_declaration`, which drops a C++ parameter pack
from the parameter list and so makes such a function nullary to the arity check.

With every fix above applied, the eval passes 90 of 90 checks, all three corpora are bit-identical
across three runs at `--jobs 8` and one at `--jobs 1`, and all three produce output byte-identical
to the pre-review branch — so none of these faults was reachable on the corpora, and they are
hardening rather than measured recoveries.

**Second review pass fixes, 2026-09-09 (`--explore`, #59):**

A multi-agent review of the branch confirmed three more issues, each fixed with a
regression test observed failing first.

*A variant repeated the base's non-`preprocess` reports.* The base merge
short-circuited past `insert_diagnostic` for any stage but `preprocess`, so the
key was never registered and every variant that re-lowered the same code
reported it again as new. The registration now always happens and the keep
decision is made afterwards, which leaves base behavior unchanged — two
translation units reporting the same `parse` finding still both report it —
while variants dedupe against what the base already said. On the corpora this
removes 13 duplicated reports on hdf, 2 on hiview and 40 on camera.

*A candidate whose value named another macro was judged not to open its arm.*
Evaluating an arm against only the macros its condition can reach (the change
that made the search linear) walked the closure without the candidate's own
binding in scope. For a GN entry like `X=FOO` with `#define FOO 5` in the base
environment, the closure of `X == 5` stopped at `X`, `FOO` was left unbound, and
the arm read as closed — so the goal could not pack into an existing variant and
either burned a budget slot or was truncated. The pending binding now takes part
in the walk. This affected only packing, never a variant's real preprocessing,
which always inherits the full environment.

*Return-flow deduplication ran on every merge mode.* Only a variant re-lowers a
body the program already holds, so only a variant can repeat a return flow; the
linear scan is now gated on that. The default hdf export is byte-identical
either way, which is the evidence that the scan was rejecting nothing on the
base path.

Re-validated after these fixes: **622 workspace tests** (three more regressions:
a variant not repeating the base's report, a different stage still being kept,
and an aliased candidate packing into an existing variant), strict all-target
Clippy, `rustfmt --check`, `RUSTDOCFLAGS='-D warnings' cargo doc`, whitespace,
and **86/86** pinned corpus checks. The default hdf export is unchanged
(`57e6aaa…`), `--explore --explore-budget 0` still reproduces it byte for byte,
and hdf `--explore` is identical across three runs plus one at `--jobs 1`.
Duplicate `call_sites` groups, defined-function counts and call-site counts are
unchanged from the previous pass.

**Review fixes, 2026-09-09 (`--explore`, #59):**

A review of the branch found two defects that contradicted guarantees this
change had written down, plus a search that grew quadratically. All are fixed
and measured here.

*Variant facts were recorded once per variant instead of merging.*
`Function::locals` had no writer outside the variant path — lowering tracks
scope in its own map and leaves the field empty — so the merge's local index
was seeded from an empty list. Every variant re-allocated a `VarId` for a local
the base already had, and calls binding those locals stopped comparing equal.
A second cause was independent: lowering names synthesized temporaries after the
unit-local `VarId` it just allocated (`_gep6` in the base, `_gep772` in a
variant), so a name could never match across configurations. Locals are now
recorded on their function in every body-merging mode, and temporaries are
paired positionally — the k-th temporary of a kind at a source position.

Groups of byte-identical `call_sites` rows, `--explore`, before → after:

| corpus | duplicate groups | `call_sites` rows |
|---|---|---|
| hdf | 7,593 → **16** | 81,707 → 73,971 |
| hiview | 2,436 → **172** | 37,972 → 35,644 |
| camera | 18,281 → **217** | 120,393 → 102,105 |

The default configuration has **0** on all three. What remains are sites in
headers where the recorded facts genuinely differ across configurations, which
`SQLITE_SCHEMA.md` documents as separate records; the systematic repetition is
gone. A consumer counting `call_edges` no longer reads that repetition as
recovered coverage.

*A variant could evict a baseline fact.* An `#ifdef X / #else` pair spells one
function's two implementations on *different lines*, so the line-keyed dedup
missed and `add_function_with_param_types` treated the variant's body as a
redeclaration — overwriting the surviving definition's span and parameters. On a
two-arm fixture the exported span for `install` moved from the arm the default
configuration compiles (lines 9–11) to the variant's (5–7), and the baseline's
indirect edge at 10:5 was **replaced** by one at 6:5 rather than joined by it.
That contradicts the may-analysis invariant and the branch's own comment at
`pag.rs` that merging a variant only ever adds. A variant definition now extends
the base entry it belongs to, found by file and name; a name several defined
functions in one file share (C++ overloads) does not identify a function and
keeps the line-keyed behavior.

*The search was quadratic in candidate arms.* Each placement rebuilt the whole
macro environment and re-preprocessed a conjunction of every arm secured so far,
and the placement loop rescanned the variant's defines and targets. The budget
capped variants, not work. Arms are now re-checked only when the added define
can reach them — decided from the identifier closure of their conditions,
through macro aliases, so a miss is a proof rather than a guess — each condition
is evaluated against only the macros it can reach, and the loop's membership
questions are hash lookups.

One translation unit, all arms mutually compatible so they pile into one
variant:

| candidate arms | before | after |
|---|---|---|
| 800 | 1.19 s | 0.07 s |
| 1,600 | 1.54 s | 0.13 s |
| 3,200 | 5.89 s | 0.26 s |

Linear in the arm count, and the result is unchanged: both recover all 3,200
conditional functions.

Also fixed: the diagnostic dedup key ignored `stage`, so a variant's `parse`
report could stand in for a later unit's `preprocess` report at the same
position; `union_aggregate_layout` mutated a type in place without letting the
tag maps reconsider it; and `variants_merged` was written to `options_json`
undocumented.

Fresh validation on macOS, `--jobs 8`, solver budget 800,000, against the three
pinned checkouts under `/private/tmp/corpora`:

- `cargo test --workspace`: **619 passed, 0 failed** (613 before, plus six
  regressions: duplicate-free configuration-independent call sites, a preserved
  base definition and its call site, alias reachability in the dependency
  closure, variant bindings in that closure, stage-aware diagnostic dedup, and
  tag richness after a union). The first two were observed failing before their
  fixes and passing after.
- Strict all-target Clippy, `rustfmt --check` on every touched file, and
  `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`: clean.
- `python3 scripts/eval_check.py --corpus-base /private/tmp/corpora`: **86/86**.
- The default (no `--explore`) hdf export is **byte-identical** to the same
  export before these fixes, excluding `analysis_run` — the corrections do not
  touch the configured path.
- `--explore --explore-budget 0` reproduces that default export byte for byte.
- hdf `--explore` three times plus once at `--jobs 1`: all four **identical**.

Exploration still adds what it is for, measured against the default run:

| corpus | defined functions | distinct source edges | wall | peak RSS |
|---|---|---|---|---|
| hdf | 10,246 → 10,334 | 75,676 → 76,631 | 8.72 s | 730 MB |
| hiview | 7,779 → 8,108 | 28,613 → 30,659 | 5.35 s | 391 MB |
| camera | 19,015 → 19,194 | 73,507 → 75,290 | 22.26 s | 1,030 MB |

Peak RSS against the default configuration on the same machine — hdf 649–686 MB,
hiview 360–374 MB, camera 1,093–1,198 MB — puts every exploring run inside the
baseline's own run-to-run range, so #59's memory budget is met rather than
assumed. Distinct source edges group by caller, callee, source file/line/column
and resolution.

**Cleanup verification, 2026-09-09 (`--explore`, #59):**

Reviewed the cross-crate cleanup and retained the guarded direct call-site lookup,
borrowed token rendering, removal of the parameter-vector clone, macro-read/goal
deduplication, and disabled LineMap tracking for synthetic predicates. These
remove identifiable work; no controlled timing comparison was performed, so this
pass makes no throughput claim. Iterator rewrites, annotations, and formatting
are readability/API changes rather than measured performance gains.

Two proposed optimizations needed correction:

- Filtering with `to_str().is_some_and(...)` skipped entire non-UTF-8 directory
  names. Filename-byte filtering now preserves them while excluding hidden names
  and `target`. A Unix filename regression failed before the fix and passes now.
  `to_string_lossy()` already borrows valid UTF-8 names, so replacing it did not
  eliminate an allocation for every ordinary directory entry.
- `opts.defines.extend(base_defines.clone())` allocated a temporary tree map.
  Cloning entries directly into the destination avoids that intermediate map.

New unresolved `LineMap` documentation links were qualified. Workspace Rustdoc
also exposed older unqualified method links and a public-to-private link; those
were corrected without changing runtime behavior.

Fresh verification: **613 workspace tests passed**, strict all-target Clippy
passed, and `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps` passed.
The rebuilt release binary passed **86/86 pinned corpus checks**.
Formatting and diff-whitespace checks passed. Earlier timing tables below remain
historical measurements, not measurements of this cleanup.

**Second review pass, 2026-09-08 (`--explore`, #59):**

Three fixes, each measured as a controlled before/after on this machine — the
same binary flags, the same solver budget, one run each — rather than against
the table below, which was captured under different conditions and does not
reproduce row-for-row here.

- `#else` arms were unreachable. Activation candidates were drawn from the
  target arm's own `reads`, and the preprocessor records none for an `#else`:
  it opens by *falsifying* the arms above it. `#ifndef FOO / #else` — one of
  the most common ways an alternate implementation is spelled — was therefore
  never explored at all, and `ArmDirective::Else` in `ArmCondition::from_chain`
  was dead code. Candidates now come from the whole chain prefix; the
  activation check already negates every preceding arm, so a candidate that
  would keep an earlier arm true is still rejected.
- Enabling exploration could *lose* a baseline fact. In
  `ensure_field_summary_for_var_named`, a GEP whose `expected_name` resolved
  nowhere in the base variable's layout returned `None`, where the baseline
  path returns the positional field's summary. Merging a variant must only add
  facts; it now falls back to the positional entry.
- A variant re-lowers the whole unit, so every non-`preprocess` diagnostic the
  base already reported came back once per variant. Variant diagnostics are
  deduplicated at every stage now; one a variant's own code produces is still
  new, and still kept.

Also: variant call records no longer overwrite the program-wide site-key entry
for a source location, so the base configuration's record stays canonical for a
later unit that reaches the same site; `union_aggregate_layout` answers the
common "this variant adds nothing" case against the stored layout instead of
deep-cloning a descriptor per existing field first.

| corpus | functions | call-edge rows | distinct source edges | flow-edge rows | variants |
|---|---:|---:|---:|---:|---:|
| hdf | 12,771 → 12,771 | 85,033 → 85,374 | 76,630 → 76,631 | 153,774 → 154,592 | 86 → 87 |
| hiview | 11,848 → 11,848 | 32,688 → 32,760 | 30,655 → 30,659 | 66,579 → 66,656 | 59 → 61 |
| camera | 25,892 → 25,892 | 82,965 → 83,017 | 75,291 → 75,291 | 126,384 → 126,460 | 109 → 109 |

Every metric is up or unchanged; nothing is lost on any corpus. Camera's variant
count is unchanged because its budget is already saturated — its gain is the
field-summary fallback alone, which is the clearest evidence that the third
finding was a real loss and not a theoretical one.

- `cargo test --workspace`: **612 passed, 0 failed, 0 ignored**.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`
  on the touched crates: passed.
- `python3 scripts/eval_check.py --corpus-base /private/tmp/corpora`: **86/86**.
- HDF `--explore --explore-budget 0` versus the default run: the two exports are
  byte-identical once `analysis_run` is excluded.
- HDF `--explore` three times plus once at `--jobs 1`: all four exports
  byte-identical. The index stays reproducible.

**Local review validation, 2026-09-08 (`--explore`, #59):**

The review fixed two losses of variant facts: grouping `A && !B` with `B` could
close a targeted arm, and merging calls by source location alone discarded
macro-selected callback arguments. Candidate predicates now include earlier-arm
exclusions and are rechecked under combined definitions. Call merging retains
separate argument/receiver/return facts. Local-variable matching uses a hash index
for the functions being extended, replacing repeated linear scans; call lookup
and flow deduplication remain scoped to one TU's variants. Base indexing reuses
the existing path instead of duplicating its lowering/error handling.

Fresh validation on macOS, the three clean revision-pinned corpora under
`/private/tmp/corpora`, `--jobs 8`, solver budget 800,000:

- `cargo test --workspace`: **605 passed, 0 failed, 0 ignored**.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Rebuilt release binary: **86/86** default `eval_check` checks passed.
- HDF `--explore --explore-budget 0`: every analysis table matches the default
  export row for row; `analysis_run` is excluded because requested options and
  run metadata differ.
- Regression tests observed failure before the corresponding fixes and pass now:
  incompatible define grouping, shadowed `#elif` feasibility, and changed callback
  arguments at a shared call site. Identical shared calls still deduplicate.

| corpus | functions | call-edge rows | distinct source edges | flow-edge rows | wall seconds |
|---|---:|---:|---:|---:|---:|
| hdf | 12,771 | 85,105 | 76,630 | 154,653 | 11.33 |
| hiview | 11,848 | 32,966 | 30,655 | 72,811 | 6.71 |
| camera | 25,892 | 83,153 | 75,291 | 137,102 | 30.28 |

These are one fresh run per corpus with `--explore` at budget four; timings include
`cargo run` startup and are not a controlled before/after speed comparison. The
larger raw edge counts include separate call records for differing variant facts,
so they must not be read as equivalent gains in source-level coverage. Distinct
source edges group by caller, callee, source file/line/column and resolution.
Conditional signatures and multi-define/nested activation remain the documented
[analysis limitations](ANALYSIS.md#limits-of---explore).

Reproduce the default check with
`python3 scripts/eval_check.py --corpus-base /private/tmp/corpora` after rebuilding
`cargo build --release -p trace-cli`. For exploration, run
`TRACE_SOLVE_BUDGET_POPS=800000 cargo run -p trace-cli --release -- analyze <corpus> --jobs 8 --explore -o <output.db>`.

**Original implementation measurements, 2026-09-08 (`--explore`, #59):**
The following numbers describe the implementation before the local review fixes;
the fresh validation above supersedes them for the current tree.
release build of the branch against the three pinned checkouts under `/private/tmp/corpora`,
`--jobs 8`, 800,000-pop budget. The default path passes **86/86** `eval_check` checks: `--explore`
is off by default. Budget-zero equivalence concerns analysis facts; `analysis_run`
records different exploration options and must be excluded from a comparison.
That exactness needed a fix. The solver's name-based GEP fallback was gated on the `--explore`
flag, so asking for exploration relaxed the cross-struct `FieldId` guard even when no variant was
ever generated and no layout was ever unioned: at budget 0 it admitted 11 call edges the guard
exists to reject. It is now gated on `Program::variants_merged`, the count of variant units that
actually merged.

`--explore` was run three times per corpus, plus once at `--jobs 1` on hdf. Every run is
**row-identical** — the index stays reproducible, which the shared include-expansion cache makes
easy to break: variant preprocessing inherits `frozen_expansion_cache`, so an exploratory define
cannot publish an expansion for another unit to consume. Candidate discovery also walks the tree
in sorted order and breaks equal-confidence ties on the candidate's source path, so the value a
variant is built with does not depend on `read_dir` order — that one is invisible on a single
machine and would have surfaced as an unreproducible index elsewhere.

| corpus | functions | call edges | flow edges | wall (base → explore) |
|---|---|---|---|---|
| hdf | 12,649 → 12,771 | 75,679 → 76,634 | 134,838 → 146,833 | 7.6s → 9.3s |
| hiview | 11,436 → 11,848 | 28,636 → 30,677 | 58,762 → 71,048 | 5.1s → 5.3s |
| camera | 25,613 → 25,888 | 73,507 → 75,194 | 104,068 → 122,113 | 21.2s → 24.3s |

The cost is the preprocessing and lowering of the variants themselves: +22% on hdf, +15% on
camera, +5% on hiview at a budget of four.

Counted over **defined** functions, exploration only adds: +90 on hdf, +329 on hiview, +186 on
camera, with **none** of the baseline's definitions missing on hdf or hiview. Camera shows three,
and they are not a loss either — `LoggingSurfaceBufferInfo` and the two `TestCaptureCallback`
members are each defined in two twin files, and the run anchors the row to whichever merges
first; that dedup predates #59.

A raw whole-table count does suggest 29 hdf functions "dropped", and that reading is wrong. Every
one of the 29 carries `is_defined = 0`: they are call-target stubs, anchored at the site that
first references them. Exploration opens earlier code in the same file, so the first reference
moves and the stub is re-anchored — `HDIServiceManager::GetService` goes from line 730 to line 90
of `service_manager_hdi_c_test.cpp`, and the same file gains 18 recovered test bodies. Comparing
by name, all 29 are still in the database. Worth stating because the naive query looks like a
regression and is not one.

Review fixes measured against the first cut of the branch (hdf): pairing variant parameters with
the base signature by name rather than by position removed **4,225** variables that positional
pairing had duplicated or bound to the wrong parameter. The same change stops a variant-only
parameter from growing the canonical arity, which the solver reads as an indirect-call target's
parameter count — growing it would have cost baseline call edges.

**Initial validation, 2026-09-08 (dependency roots `--dep <root>`, #60):** fresh release builds of
`master` (be7d7ca) and the branch were compared against the three clean pinned checkouts under
`/private/tmp/corpora`, `--jobs 8`, 800,000-pop budget. Both pass **86/86** `eval_check` checks
with every measured value identical.

The corpora are analyzed without `--dep`; this checks that single-tree analysis
results remain unchanged. It does not measure dependency-mode performance.
Dumped sorted from each pair of databases, the `call_edges` rows (caller, call site, callee,
resolution) and the `functions` rows (name, line range, linkage, `is_defined`) are **byte-identical
across all three corpora** — 75,679 / 28,636 / 73,507 edge rows and 12,649 / 11,436 / 25,613
function rows for hdf, hiview and camera. Every file and function in those runs carries
`is_dep = 0` and `options_json.dep_roots` is `[]`, which is what the code predicts: with no root
declared, `SymbolTable::dep_roots` is empty, `FileInfo::is_dep` is false for every interned path,
and dependency guards evaluate to false.

Dependency behavior itself is pinned by fixture rather than by corpus, in
`tests/fixtures/dep_root/` and `crates/trace-cli/tests/dep_root_tests.rs`: a source under the
dependency root stays out of the index, an unreached dependency header is never an orphan unit,
`sptr<TargetService>` unwraps through the declared `operator->` to direct edges on
`TargetService`, no edge lands on the wrapper, no function declared in a dependency header is
ever a *caller*, a target call to a dependency declaration does resolve, and `--exclude-deps`
drops exactly the edges that touch a dependency function and no others.

Review follow-up: the original merge-only filter retained function-local statics
and could retain body flow through parameters or globals. A regression reproduced
the leaked static before the fix. Dependency function bodies, constructor-initializer
lists, and variable initializers now stop during lowering, before body IR is
allocated. This removes work rather than building facts and filtering them later;
no wall-time or memory speedup is claimed here.

Fresh validation after the follow-up: **584 workspace tests** pass;
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --all --check` are clean. The five dependency tests cover the original
resolution/export/CLI behavior plus body and initializer exclusion with one and two
workers, and preservation of target-header bodies reached through dependencies.
A fresh release build also passes **86/86 corpus checks** at the same pinned
checkouts (`python3 scripts/eval_check.py --corpus-base /private/tmp/corpora
--outdir /private/tmp/dep-review-eval`). These follow-up runs use no `--dep`;
the row-by-row comparison was also independently re-run against the reviewed
implementation (`084a498`, squashed into this commit).
The sorted `call_edges` and `functions` dumps remain **byte-identical to master
(be7d7ca) on all three corpora** after the lowering-path change. Unchanged
no-`--dep` analysis output is therefore measured for the reviewed implementation,
not inferred from its guards.

**Re-verified 2026-09-08 (GNU `, ## __VA_ARGS__` decided from the body, #65):** fresh
release builds of `master` (c4d0ff0) and the branch were compared on the same machine against
the three clean pinned checkouts under `/private/tmp/corpora`, `--jobs 8`, 800,000-pop budget.
Both pass **86/86** `eval_check` checks with every measured value identical, and the SQLite dumps
excluding `analysis_run` are **byte-identical** on all three corpora (637,396, 339,509 and
696,324 statements). The `parse_failures` captures (633 / 142 / 385 rows) and the
`conditional_coverage` TSVs are byte-identical too, so `docs/PARSE_FAILURES.md` and
`docs/CONDITIONAL_COVERAGE.md` need no regeneration and `scripts/eval_expected.json` needs no
re-capture.

The change does move output on a shape the corpora do not contain, which is why it was measured
rather than assumed: `, <param> ## <variadic tail>` — a comma, then a parameter, then the paste —
now keeps its comma (gcc's reading) where it used to delete it. Scanning the three trees for it
(logical `#define` lines, `\`-newline continuations joined) finds three `, <ident> ##` macros in
all, the same three #54's eval notes reported: `HCS_NODE`, `HCS_PROP` and `HCS_NODE_HAS_PROP` in
HDF, whose left operand is the literal `_` and which are not variadic. The variadic logging
macros the trees do define paste `## __VA_ARGS__` straight onto a comma, which is the form and is
unchanged.

**Re-verified 2026-09-08 (conditional `#include` suppression, #56):** fresh
release builds of `master` (0a35409) and the branch were compared on the same
machine against the three clean pinned checkouts under `/private/tmp/corpora`,
`--jobs 8`, 800,000-pop budget. Both pass **86/86** `eval_check` checks with
every measured value identical, and the SQLite dumps excluding `analysis_run`
are **byte-identical** on all three corpora (637,396, 339,509 and 696,324
statements). The `parse_failures` captures (633 / 142 / 385 rows) and the
`conditional_coverage` TSVs are byte-identical too, so `docs/PARSE_FAILURES.md`
and `docs/CONDITIONAL_COVERAGE.md` need no regeneration and
`scripts/eval_expected.json` needs no re-capture.

That the corpora do not move is the expected result, not an absent effect:
every cacheable header the three trees reach carries a recognised guard or a
`#pragma once`, so the skip decision is the same one the blanket path set used
to make. What changes is which *reasons* are accepted, and the reachable
regression was the reverse direction — suppressing an include for a reason
that does not hold there. Two shapes of that were found and fixed before this
result: fingerprinting the guard name at a skip site (camera −879 direct
edges, since an entry then only matched a consumer with the same include
history and nothing published past `max_expansion_variants`), and exporting
`IncludeExpansion` guards for files the entry does not cover (camera −1,064
direct edges, consumers suppressing an `#include` whose body they never
received). Both are recorded at their sites in `preprocessor.rs`.

Index time and peak RSS, three runs each on macOS (Darwin), 8 logical CPUs,
`--jobs 8` — the change deliberately re-expands more than before, so these are
budgets rather than a formality:

| Corpus | `master` time | branch time | `master` peak RSS | branch peak RSS |
|--------|---------------|-------------|-------------------|-----------------|
| drivers_hdf_core | 7.67–7.74 s | 7.68–7.77 s | 602–685 MB | 672–673 MB |
| hiviewdfx_hiview | 5.04–5.12 s | 5.11–5.23 s | 357–361 MB | 348–360 MB |
| multimedia_camera_framework | 21.20–23.19 s | 21.19–21.53 s | 1,056–1,121 MB | 1,077–1,160 MB |

Every branch range overlaps `master`'s, so the cost of re-expanding is inside
run-to-run noise on these trees. The index stays reproducible: three `--jobs 8`
runs and one `--jobs 1` run of each corpus give the same dump hash.

**Re-verified 2026-09-08 (language predefines `__cplusplus` / `__STDC__` /
`__STDC_VERSION__`, #70):** fresh release builds of `master` (ef4cf1e) and the
branch were compared on the same machine against the three clean pinned
checkouts under `/private/tmp/corpora`, `--jobs 8`, 800,000-pop budget. Both
pass **86/86** `eval_check` checks with every measured value identical, the
SQLite dumps excluding `analysis_run` are byte-identical on all three corpora
(637,396, 339,509 and 696,324 statements), and the `parse_failures` captures
are byte-identical (hdf 740, hiview 3,190, camera 3,412 rows). A `--jobs 1`
run of the branch on HDF dumps identically to its `--jobs 8` run.

The index does not move because every `__cplusplus` conditional in these
corpora — 930 of them — guards only an `extern "C" {` / `}` wrapper; not one
compares the value or guards a declaration. Those wrappers now reach the C++
parser as `linkage_specification` nodes, which changes no symbol, span, edge
or diagnostic.

What moves is the measurement that found the defect,
[CONDITIONAL_COVERAGE.md](CONDITIONAL_COVERAGE.md), regenerated here on all
three corpora (4,742 chains, unchanged). `__cplusplus` was read by 930 chains
with **every** evaluation unbound; the reads under C++ units are bound now, and
what stays unbound is the C units and the headers reached only from C.

| Coverage metric | `master` → #70 |
|---|---|
| HDF lines always excluded | 7,302 (2.4%) → 6,980 (2.3%) |
| HDF lines excluded in some runs only | 80 → 387 |
| HDF `__cplusplus` evaluations bound | 0 / 18,432 → 1,518 / 17,028 |
| HDF files with an always-excluded arm | 446 → 341 |
| Camera lines always excluded | 5,031 → 4,991 |
| Hiview lines always excluded | 7,397 → 7,387 |

The `__cplusplus` rows drop out of the Hiview and Camera unbound-name tables
entirely: there every read is now bound. The recovered lines are the wrapper
bodies themselves, which is why the recovery is visible in coverage and not in
the index.

**Re-verified 2026-09-08 (#57 review follow-ups):** conditional records now count a
final comment/whitespace line without a trailing newline, locate spliced directives
at their opening `#`, and retain operator-shaped names used as explicit macro
operands. The report reader requires a final row-count completion record, rejecting
captures cut off between complete TSV rows. Regression tests cover all four cases.

Rebuilt the release example and regenerated all three clean pinned corpus captures
and [CONDITIONAL_COVERAGE.md](CONDITIONAL_COVERAGE.md): all **4,742 chains** and
coverage totals are unchanged. A fresh release CLI passes **86/86 corpus checks**
without changing expectations. **531 Rust tests**, **4 Python reader tests**, Clippy
with warnings denied, and formatting checks pass.

**Re-verified 2026-09-07 (conditional-compilation coverage report, #57):** fresh
release builds of parent `b7e070b` and the reviewed branch were run on all three
clean pinned corpora under `/private/tmp/corpora`, `--jobs 8`, with the
800,000-pop solver budget. SQLite dumps excluding `analysis_run` metadata are
identical for HDF, Hiview and Camera (637,311, 339,424 and 696,239 SQL statements).
The default index does not enable conditional recording, and no analysis or
parse-failure counts change relative to the parent.

The initial evaluation found **18 inherited expectation failures**, reproduced
on the parent: the checked-in baseline predated the counts already documented
below for #64. Those failing expectations in `scripts/eval_expected.json` were
refreshed to the verified counts, with all tolerances and dispatch target
checks retained. The refreshed evaluation passes all 86 checks.

[CONDITIONAL_COVERAGE.md](CONDITIONAL_COVERAGE.md) was regenerated on all three
corpora after fixing external-header file totals and definition evidence from
comments or string literals. The report tool now also rejects missing/empty
source trees and hard input failures, retains `-D` values in metadata, and
preserves carriage returns inside TSV fields. Workspace tests, the example's
regression tests, the Python reader regression, Clippy and formatting checks
pass.

**Re-verified 2026-09-07 (member access through a declared `operator->`, #64):**
fresh release builds of `master` (52fd920) and the branch were compared on
the same machine against the three clean pinned checkouts under
`/private/tmp/corpora`, `--jobs 8`, 800,000-pop budget. The branch index is
bit-reproducible: three `--jobs 8` runs and one `--jobs 1` run print the
same totals on each corpus.

| Metric | hdf `master` → #64 | hiview `master` → #64 | camera `master` → #64 |
|---|---:|---:|---:|
| Functions defined | 10,246 → 10,246 | 7,779 → 7,779 | 19,015 → 19,015 |
| Functions external | 2,499 → 2,403 | 3,666 → 3,657 | 6,906 → 6,598 |
| Direct edges | 37,632 → 42,183 | 8,204 → 8,204 | 20,310 → 20,315 |
| Indirect edges | 4,642 → 4,642 | 24 → 24 | 109 → 109 |
| External edges | 29,853 → 28,854 | 20,316 → 20,394 | 52,898 → 53,081 |
| Arg-flow edges | 62,712 → 65,960 | 9,517 → 9,517 | 17,233 → 17,237 |
| Diagnostics | 1,803 → 1,803 | 2,989 → 2,989 | 4,860 → 4,860 |

Comparing distinct direct edges by (file, line, column, caller, callee)
loses **none** in any corpus; hdf gains **4,551**, hiview **0**, camera
**2**. Every dispatch-target, dlsym, IPC and probe check passes on both
builds; indirect edges and diagnostics are identical.

hdf is where the change lands: HDI's in-tree `OHOS::HDI::AutoPtr<T>`
declares `T *operator->()` and was on no name list, so every `p->m()` on an
`AutoPtr` invented `AutoPtr::m`. The **118** external functions under
`OHOS::HDI::AutoPtr::*` on `master` are gone; the **36** direct calls to the
real `AutoPtr::Get` (87 to `AutoPtr::*` members overall) stay, because the
wrapper keeps its own class for `.`. Source-checked:
`CClientProxyCodeEmitter::EmitProxyMethodImpl`
(`framework/tools/hdi-gen/codegen/c_client_proxy_code_emitter.cpp:271,275`)
takes `const AutoPtr<ASTMethod> &method`; `method->GetName()` was an
external `AutoPtr::GetName` and is now the in-tree `ASTMethod::GetName`.
An earlier, type-wide draft of this fix moved hdf by +713 direct edges; the
difference is wrapper-typed fields declared in a header other than the
wrapper's, which that draft could not follow and this one does through
`Program::arrow_returns`.

hiview and camera declare no wrapper in the tree (`std::shared_ptr`,
`sptr`), so every `->` takes the path it took before. The two added camera
edges are `success->Executing()` / `failed->Executing()` on
`std::shared_ptr<VideoProcessSuccessFuzz>` locals in
`test/fuzztest/videoprocesscommand_fuzzer/video_process_command_fuzzer.cpp`
(lines 79, 86), which now reach the in-tree base-class `Executing` where
`master` had an external edge on the fuzz subclass. What moves otherwise is
the `.` side of the name fallback. A `shared_ptr` used to intern as a
pointer to its pointee, so `sp.get()` / `wp.lock()` / `up.release()` were
phantoms on the pointee (`Event::get`, `Plugin::lock`) and `sp.reset()`
bound to a real `reset` on the pointee when it had one; they are external
calls on `std::shared_ptr::get`, `std::weak_ptr::lock`,
`std::unique_ptr::release` now, hence fewer external *functions*. External
*edges* rise because a wrapper is now a class value: a member initializer
or local declaration of one emits a constructor call
(`std::shared_ptr::shared_ptr`, `std::unique_ptr::unique_ptr`) — 135 hiview
sites gain such an edge, 57 lose a pointee phantom. Camera also loses
`std::vector::iterator::GetCameras` / `::OnCameraStatus`, the members
`master` invented on an iterator for `(*it)->GetCameras()` in
`services/camera_service/src/hcamera_host_manager.cpp:1398,1400`: a
dereference of a class that declares no `operator->` is unknown now and the
site stays unresolved.

**Expectation status: `master` 72/86, branch 68/86.** The fourteen misses
on `master` are #66's — it moved every corpus's totals and diagnostics
without re-capturing `scripts/eval_expected.json` — and the branch adds the
four bands the change legitimately leaves: hdf direct and total edges,
camera total and external functions. The values are not re-captured here
because the baseline is already off; re-capture once on the reference setup
after this lands. Every exact check that passes on `master` passes on the
branch.

Validation: **501 workspace tests** pass, **119** of them C++ cases;
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --check` are clean.

To reproduce, build `trace-cli --release` from each tree, then run each
binary with:

```sh
python3 scripts/eval_check.py --bin /path/to/trace \
  --corpus-base /private/tmp/corpora --outdir /tmp/arrow-eval
```

**Re-verified 2026-09-06 (empty left token-paste operand, #52):**
compared fresh release builds of the parent (#38, compared pre-merge as
`e763ff2` and merged unchanged as `240bb97`) and the local #52 fix
against all three clean OpenHarmony checkouts at the revisions in
`scripts/eval_expected.json`, using `--jobs 8` and the 800,000-pop budget.
`eval_check.py` passes **86/86** on both (including three checkout checks).
All global metrics, dispatch target counts and probe results are identical
between these runs; no expectation values needed adjustment.

The parse-failure TSVs produced by each build's `parse_failures` example
from its corresponding evaluation databases are byte-identical in all
three corpora. Regenerating `docs/PARSE_FAILURES.md` produces no diff: the
same **259** files, ERROR sites, output rows, columns and snippets. Thus
#52 has no measured impact on these pinned corpora. Its regression fixture,
`tests/fixtures/preproc/empty_left_paste.c`, checks stringized adjacency and
the non-stringized output that feeds parsing: `a x ## "s"` must retain the
string literal when `x` is empty, and `f(x ## y)` with empty `x` must retain
the opening parenthesis as a separate token. Both non-stringized cases fail
on the parent build and pass with the fix; Clang agrees with the fixture's
expected output.

The corpus runs above predate the review follow-up that keeps an empty
parameter between a comma and `## __VA_ARGS__` out of GNU comma elision's
way. That guard cannot move them: it fires only on a macro body whose `##`
has a parameter on its left, the variadic tail on its right, and a comma
before that parameter, and no such body exists in the three checkouts --
`#define`s matching `, <identifier> ##` at all number three, all in
`drivers_hdf_core/framework/include/utils/hcs_macro.h`, and all with the
literal token `_` rather than a parameter as the left operand
(`HCS_CAT(parent, _##node)`). Outside that shape the guard is inert and the
byte-identical results above stand.

**Re-verified 2026-09-06 (line splicing in the lexer, #38):** all three
pinned corpora were re-fetched at their pinned revisions and analyzed with
the branch binary and with one built from `master` (80c2325). `eval_check`
passes 83/83 with both, and every exact metric — `diagnostics`,
`edges_indirect`, `dlsym_edges`, every dispatch target set and every probe —
is identical between the two runs, as are the bulk totals. The parse-failure
set is unchanged: the same **259** files with the same ERROR sites, so the
shapes the fix targets (an identifier or multi-character punctuator written
across a `\`-newline, the spliced `...` of #38) do not occur in these corpora
outside directives and are covered by unit tests only.

What does move in `docs/PARSE_FAILURES.md` is the **line** of 31 sites in
eight files (seven HDF, one Hiview), each 1–3 rows earlier. That column is
tree-sitter's row in the *preprocessed output*, and a `\`-newline in ordinary
code — a string literal continued across lines, a statement ending in `\` —
used to be written back as the `\` token plus a newline, one extra output
line per continuation. The lexer now deletes it in translation phase 2, so
every site below such a continuation shifts up by the number of continuations
above it. The sites themselves, their columns and their snippets are the
same. The committed report was also eight sites stale against `master`'s own
binary — the `->*` sites in camera's two event-emitter headers and `dps.h`
had been catalogued as `->* func` fragments and are `->` / `*` fragments
under the current `master` tokenization — so the regenerated file carries
that drift too; it is attributed to `master` by regenerating from the
`master` binary's TSVs, which reproduce it exactly.

The cost is in the lexer alone: lexing every file of the camera corpus
single-threaded (16.1 MB, best of five) goes from **123 to 112 MiB/s**, the
per-character splice check, and the token count drops by 836 — the `\` +
newline pairs that no longer exist as tokens.

**Re-verified 2026-09-05 (review follow-ups on #46):** five further review
passes over the branch found eleven more shapes, and all three corpora were
re-analyzed before and after fixing each. Ten of the eleven do not occur in
the pinned corpora and move nothing — every global metric, dispatch check and
probe byte-identical across those runs — so, as with the three shapes the first
pass found the same way, they are covered by unit tests only.

The eleventh is the largest single correctness gain on this branch, and it needs
no error recovery at all to happen. A standard or GNU attribute
(`[[deprecated]]`, `__attribute__((pure))`) parses perfectly well, but it holds
an identifier of its own in front of the declaration and the member walk took
it — so **every annotated member of a class collapsed into one symbol**.
Conversion operators are what made it reachable: their declarations had never
registered before this branch, so the walk had never been asked to name one.
In camera, `interfaces/inner_api/native/camera/include/input/camera_input.h`
annotates most of `CameraInput` with a bare `[[deprecated]]`, and the whole
class came down to a single `CameraInput::deprecated`. Fixing it removes
**7** such phantoms (`CameraDevice`, `CameraInput`, `CameraManager`,
`HCameraService`, `PhotoOutput` under `deprecated`; `SteadyClock`,
`TaskManager` under a GNU `visibility`) and restores **26** real members — 25
of `CameraInput` and `CameraManager::GetCameraInfo`. Net **+19** functions, all
external (they are prototypes; none of the 26 has a definition in this tree),
so camera moves to **25,904** total / **6,930** external with `functions_defined`
unchanged at 18,974, and edges, arg-flow and diagnostics unchanged. hdf and
hiview hold no attribute-named member either before or after and do not move.
The bands are ±120, far too wide to have caught 19, so `eval_expected.json`
gains an exact probe per corpus pinning attribute-named members at zero, plus
one pinning `CameraInput::LockForControl` under its own name.

Three were error-recovery shapes the first pass's rules read the wrong way
round. An unknown attribute macro in front of a *conversion* operator strands
the `operator` keyword in an `ERROR` and leaves the target type in declarator
position, which in a class body named the member after its target
(`MACRO operator ns::S() const;` → `C::S`) and out of one looked exactly like
the fabricated qualification of `FFI_EXPORT void C::M()` — so `EXPORT
C::operator int() {}` had its real class cut off the front and escaped to
global scope, stranding the declaration it should have merged with. The two
repairs are told apart by where the `ERROR` sits relative to the `::`: before
it the scope is a leftover type, after it the scope is real. The third:
a nullary declarator with a *trailing* macro (`void C::M() OVERRIDE {}`)
parses as a call, so tree-sitter parks the real declarator in an `ERROR` and
hands the `declarator` field to the macro — the definition landed on a
*defined* function named `OVERRIDE` (one per class annotating a nullary
member, all merging into one symbol) while `C::M` stayed undefined and its
body unreachable. A declarator with parameters parses fine and was never
affected, which is why the corpora show nothing: their annotated members take
arguments.

Two more were the same blind spot in both of those repairs: each was looked for
only among a `qualified_identifier`'s *direct* children, but a qualified name
nests one level per scope it carries and recovery marks the level the
fabricated segment landed on — so every scope either half of the name spells
pushes that mark further from the top. `FFI_EXPORT n::S C::M() {}` was still
indexed as `n::S C::M`, and `EXPORT ns::C::operator ns::S() {}` split into a
defined `ns::C::operator ns::S` beside the undefined `ns::C::operator S` it
should have merged with. Both are now searched down the whole chain, which
costs nothing: the scope and the target are read by byte offset around the
mark, so its depth never mattered to anything but finding it.

Two were failures of the rule that tells the leading attribute-macro repair
from the trailing one. Wearing both at once
(`EXPORT_API int Get(long) GUARDED_BY(mu_);`) puts the leftover return type and
the real declarator in the *same* `ERROR`, so "does this `ERROR` hold a
declarator?" answered yes and the walk read the whole node, taking the type
first — `C::int` and `C::void`, one per return type in the class, with the real
members surviving only as call-site externals. Reading only the `ERROR`'s
declarators fixes it, and a conversion operator wearing both macros needed the
converse: its target is swallowed by that same `ERROR` while the trailing macro
sits outside, so reading the first thing *after* the `ERROR` named it
`C::operator GUARDED_BY`. (The other is the attribute collapse described
above.)

Two more concerned the conversion target's own spelling, and both were
verified to leave every corpus symbol byte-identical. Dropping *every* scope
from the target made `operator a::S` and `operator b::S` one member, two
bodies under one symbol; dropping only the scopes the member itself sits in
keeps the merge that motivated the stripping (`ns::Handle::operator ns::S`
still meets the in-class `operator S`) while a scope the member does not sit
in — which no spelling could have elided — is kept. Template arguments are
kept for the same reason, so `operator Vec<int>` and `operator Vec<double>`
stop colliding; the declaration and its out-of-class definition differ in
scope rather than in arguments, so nothing that used to merge stops. Neither
of the corpora's two conversion operators has a qualified or template target,
which is why nothing moves. Separately, `operator void (*)()` recorded a bare
`Ptr(Void)` — indistinguishable from a conversion to `void *`, so nothing
downstream could see the target as callable; it now lowers to `Ptr(FnPtr{..})`,
the descriptor the `typedef`ed spelling of that same type already produced.

Canonicalizing that target took five corrections in review, each hidden by
the fix before it, because its pieces interact: a leading `::`, nested
enclosing scopes, template arguments, repeated segments, and class-relative
spellings. Rather than a case per defect, `lower.rs` now carries a property
test that enumerates every legal C++ spelling of every type in a generated
world of scopes and asserts the two things the naming exists to do — all
spellings of one type name one member, and no two types name the same one.
It found the fifth defect (`H::T` from inside `n::H`) on its first run.

Two shapes were deliberately left alone, both recorded in `docs/ANALYSIS.md`.
An annotated *data* member (`int a_ GUARDED_BY(mu_);`) still contributes one
phantom `Cls::GUARDED_BY`: it parses as type field, `ERROR`,
`function_declarator` — the identical shape to a *function* behind a leading
macro (`MACRO int Plain() const;`), with the two halves meaning opposite
things, so suppressing the phantom drops real methods instead. That was tried
and reverted; it broke the `FFI_EXPORT CArr Get(long);` recovery this branch
adds. And a definition wearing a macro on *both* sides
(`EXPORT void C::M() GUARDED_BY(m) {}`) splits across two top-level nodes —
`C::M` in a `declaration`, the body under a `function_definition` named for the
macro — so the repair would have to span nodes rather than fix one.

The last is the `TypeTable::int()` bug's twin, one screen away in the same
file: `resolve_type_id` fell back to a raw `TypeId(5)`, which the prelude had
grown past — it names `Long`, where the function means `Unknown`. Both are
non-pointer scalars so no points-to result changes, and the new
`unknown()` accessor is pinned by the same test as `void()` and `int()`.

One measurement note, unrelated to these fixes and present before them: on this
machine hiview reports **11,456** functions (defined **7,776**, external
**3,680**) rather than the pinned 11,465 / 7,774 / 3,691. It is inside the ±120
band and reproduces exactly across job counts 1, 4, 8 and 16 and across both
runs, while `files` and every correctness number match the pin exactly — so it
is machine-to-machine variance of the kind the bands exist for, not drift and
not a regression. The pins were left alone rather than re-captured to a second
machine's numbers.

**Re-verified 2026-09-04 (conversion operators, #46):** all three corpora were
re-analyzed with the current release binary and with one built from `master`
(9979676), so every delta below is attributed rather than assumed.

tree-sitter-cpp spells `operator T()` as an `operator_cast` declarator, not as
the `operator_name` that `operator=` gets, and neither declarator-kind list in
the lowering knew that kind. A conversion operator therefore went one of two
ways: a declaration failed the member test and was never registered, while a
definition reached the generic declarator walk and came out named after the
fragment the walk happened to land on. Corpus-wide that is two symbols —
`MappedMemory::()const` in camera (`photo_process_result.h:72`) and
`AutoPtr::operatorT*()const` in hdf (`hdi-gen/util/autoptr.h:156`) — now
`…::MappedMemory::operator bool` and `OHOS::HDI::AutoPtr::operator T*`.
Neither had an incoming edge before or after: an
implicit conversion is not a call site this indexer recovers, so the cost was
one junk symbol apiece rather than a resolution failure. The counts do not
move for either.

Spelling the member `operator T` needs a space, and `normalize_qualified`
deleted **all** whitespace. Deleting is right where whitespace separates a
word from punctuation — `~ Cls`, `A :: b`, the macro-expansion gaps the
function exists for — and wrong between two words, where it is the only thing
keeping the tokens apart. Collapsing to a single space there instead is what
surfaced the second finding.

`FFI_EXPORT CArrFloat32 FfiCameraZoomGetZoomRatioRange(...)` has no `#define`
anywhere in the include path, so the macro survives preprocessing. tree-sitter
takes it as the return type, has no rule left for the real one, and recovers by
pairing the type with the name under a **MISSING `::`** — a `qualified_identifier`
that is not a qualified name. Read whole, it spelled the function
`CArrFloat32FfiCameraZoomGetZoomRatioRange`, a name no call site can match.
Reading only the `name` half fixes it.

Ten declarations across two corpora hit this, and they split in two:

- **Eight glued prototypes** — seven in camera's
  `frameworks/cj/camera/include/camera_ffi.h`, one in hiview's
  `plugins/faultlogger/interfaces/cj/faultlogger_ffi.h`. Each was a junk
  *undefined* duplicate of a definition already interned under the correct
  name, so they simply disappear.
- **Two glued definitions**, both in
  `frameworks/cj/camera/src/camera_ffi.cpp`. These had no correctly-named
  counterpart at all: `FfiCameraAutoExposureGetExposureBiasRange` and
  `FfiCameraZoomGetZoomRatioRange` were absent from the index entirely, and now
  appear, defined.

That is the whole of the count movement — hiview functions **11,464 → 11,463**
(external **3,692 → 3,691**) and camera **25,891 → 25,884** (external
**6,918 → 6,911**): ten junk symbols out across the two corpora, two real ones
in, so the *defined* counts do not move either. Call edges do not move because nothing in these
corpora calls an FFI entry point — they are called from the CJ runtime — but a
caller in the same tree would have resolved to nothing. Arg-flow edges and
diagnostics are unchanged in all three corpora, and hdf does not move at all.

Review then found that the same `<...>` stripping truncates **every operator
name containing `<`**. `operator<`, `operator<=`, `operator<<` and
`operator<=>` all came out as the bare keyword `operator`, so a class's
comparison operators collapsed into one symbol — and a *declaration* spelled
that way was dropped outright by `register_member_prototype`'s
`short == "operator"` guard. This is older than the conversion-operator work
and much more common than `operator new`: **14 rows** across the three corpora
(hdf 7, hiview 5, camera 2) carried the bare keyword. `<` no longer opens an
argument span once the segment being built is an operator name, and those 14
become correctly-spelled members —
`OHOS::HDI::AutoPtr::operator<` and `operator<=` (one symbol before),
`OHOS::Hardware::Logger::operator<<`, five in hiview, two in camera. hiview
functions move to **11,465** (defined **7,774**) and camera to **25,885**
(defined **18,974**); hdf's totals do not move, because there the collapse
renamed rather than merged. A third exact probe per corpus pins the bare
keyword at zero.

Three further shapes were found by review rather than by the corpora, and fixed
with no corpus movement at all: a target type that is itself qualified
(`operator std::string`) put the in-class *definition* at global scope, because
`qualify_decl` read the target's `::` as a scope and left the name alone —
splitting it from the declaration it should have merged with; a target type
that is a function pointer lost it, since the `(*)` sits inside the
`abstract_function_declarator` the name used to be cut at (the recorded type
now descends the same chain, so name and type agree); and inside a *class body*
the unknown attribute macro above recovers as an `ERROR` node holding the real
return type rather than as a fabricated qualified name, so the member walk took
`CArr` from `FFI_EXPORT CArr Get(long);` and lost `Get` — an `ERROR` node holds
no declarator, exactly as a `decltype` operand does not (#29). None of these
three shapes occurs in the pinned corpora, so all are covered by unit tests
only.

The bands are ±120 on those totals, far too wide to have caught ten symbols, so
`eval_expected.json` gains eleven exact probes: declarator-fragment names pinned
at zero in each of the three corpora (`operator()` is a real name and is
excluded), the two conversion operators pinned by name, the glued FFI spellings
pinned at zero in camera and hiview, and `FfiCameraZoomGetZoomRatioRange` pinned
as defined under its own name, and the bare keyword `operator` pinned at zero in
each of the three. Six camera symbols keep a space in their name for
an unrelated reason — `ParseAndCheckNumber< uint8_t>` and its five siblings are
explicit template specializations whose spelling comes from another path — and
are unchanged by this fix.

**Re-verified 2026-09-04 (`->*` punctuator, #37):** the lexer had no `->*`
token, and unlike the other multi-character punctuators it lacks, the output
spacing rule pulls its halves apart — the space before `*` after `>` is what
keeps `shared_ptr<T> &p` from gluing into `>&`. So `c->*m` was written back as
`c-> * m`, which is not C++: maximal munch makes `->*` one token, and `->` may
not be followed by `*`. It is now the second three-character punctuator.

**This does not make the construct parse.** tree-sitter-cpp knows `->*` only
as an overloadable operator name and as a fold operator — both
`struct S { int operator->*(int); };` and `(a ->* ...)` parse — and has no
rule for it as a binary operator in an ordinary expression. So pristine
`c->*m` is an ERROR site with or without the space, while `c.*m` parses.
Checked against the bundled grammar (`->*` appears in `operator_name`,
`_fold_operator` and `_binary_fold_operator`, and nowhere in the expression
rules) and confirmed by parsing all four shapes. The five camera files using
`(x->*f)(...)` failed before and fail after, and the file count is unchanged
at **259**.

The token is C++-only: gcc and clang tokenize `a->*b` in C as `->` then `*`,
so the lexer gates it on the language the way it already gates raw strings
and ud-suffixes — a header reachable from both a C and a C++ unit is warmed
once per language, and each replay has to be that language's tokenization.

What the fix buys is index precision. With `-> *` split, tree-sitter recovered
the operand as a callee, so `services/deferred_processing_service/include/base/dps.h:40`

```c++
return (server.get()->*func)(cmd);
```

interned an **external function named `func`** — the name of the enclosing
template's parameter, not a function at all — and a call edge to it. Both are
gone, and `eval_check` now pins that exactly (the bulk totals below sit inside
±120/±420 bands, so they could not have caught a regression on their own): camera functions **25,892 -> 25,891** (external **6,919 -> 6,918**,
defined unchanged at 18,973), call edges **73,137 -> 73,136**, external edges
**53,601 -> 53,600**. Nothing else in any corpus moves.

The cost is cosmetic and worth stating: catalogued ERROR sites rise
**1,129 -> 1,139**, all of it in three of the five files, because recovery
around an unknown operator fragments into more ERROR nodes than recovery
around a field access did (`camera_ability_builder.cpp` reclassifies from
`missing field_identifier` to a generic ERROR). Those ten extra sites cost
nothing measurable: the only index deltas corpus-wide are the one symbol and
one edge above.

**Re-verified 2026-09-04 (`...` punctuator, #28):** all three pinned corpora
were re-analyzed with the current release binary and with a binary built from
`master` (2af1eb1), so every delta below is attributed rather than assumed.
The lexer had no `...` punctuator: an ellipsis lexed as three `.` tokens and
the token re-speller wrote them back as `. . .`, which tree-sitter cannot
parse, so every variadic declaration produced an ERROR node. `...` is now one
`Punct` token.

Parse failures drop from **286 to 259 files** (HDF 176 → 163, Hiview 37 → 32,
Camera 73 → 64) with no file newly failing. That is exactly the `parse`-stage
diagnostics delta — HDF **182 → 169**, Hiview **37 → 32**, Camera **73 → 64**,
so the totals move **1,777 → 1,764**, **2,964 → 2,959** and **4,776 → 4,767**;
`preprocess`-stage diagnostics are unchanged at 1,595 / 2,927 / 4,703. The 111
ERROR sites whose snippet was `. . .` are gone from
`docs/PARSE_FAILURES.md`, and the "generic ERROR nodes" category falls
**248 → 221 files**.

One spelling needs a space the others do not. A preprocessing number absorbs
`.` and alphanumerics (C11 6.4.8), so an ellipsis emitted straight after one
re-lexes *into* it: the GNU case range `case 0x0300 ... 0x0307:` written back
as `case 0x0300...0x0307:` returns as the single number `0x0300...0x0307`. The
re-speller therefore keeps a space before `...` when the output already ends in
a pp-number, and only then — `Args...` after an identifier is unaffected and
stays glued. This does not clear those files: tree-sitter-c has no GNU
case-range rule at all, so `case 1 ... 10:` is an ERROR site even in pristine
source (checked directly). The three sites in
`framework/model/audio/usb/src/audio_usb_mixer.c` stay failures; what changes
is that their token stream is no longer corrupt, which the report shows as the
snippet moving from `..0x0307` to `...0x0307`.

HDF and Hiview bulk totals are unchanged; every moved number is Camera's, and
it comes from the headers that now parse — chiefly
`interfaces/kits/js/camera_napi/include/camera_napi_param_parser.h`, plus
`dps.h`, `enable_shared_create.h` and `dp_utils.h` under
`services/deferred_processing_service/`.

The direct/external swing is a **resolution flip, not a re-resolution**, and it
is worth stating precisely because the raw edge counts hide it. In `master`,
`camera_napi_param_parser.h` sat inside an ERROR node, so tree-sitter recovery
lowered `CameraNapiParamParser::AssertStatus`'s *definition* at line 186 as an
unqualified free function `AssertStatus` (`is_defined=1`). Unqualified-name
matching bound **232** call sites to that phantom as **direct** edges, while
the **38** sites that do spell the qualified member found only a declaration
and stayed **external**. With the header parsing, the definition is the real
`OHOS::CameraStandard::CameraNapiParamParser::AssertStatus`, so those 38 turn
**direct** — and the 232 fall back to a bare call-site stub
(`camera_napi_template_utils.h:83`, `is_defined=0`) and turn **external**. The
same 270 edges exist on both sides; only their classification moves.

Across all callees the pattern is uniform: five phantom unqualified names lose
**266** direct edges (`AssertStatus` 232, `IsStatusOk` 15, `weak_from_this` 12,
`Next` 5, `GetThisVar` 2) and fifteen qualified members gain **190**, net
**−76**. Two of the gains are genuinely new edges rather than requalified ones
— `DeferredProcessing::DPS_SendCommand` (27) and `CreateShared` (17) have no
rows at all in the `master` DB — and they account for most of the +84 in
`edges_total`. `weak_from_this` (12) is a pure requalification
(`DeferredProcessing::weak_from_this` →
`DeferredProcessing::EnableSharedCreate::weak_from_this`), same count on both
sides.

Net for Camera: functions **25,885 → 25,892** (defined 18,964 → 18,973,
external 6,921 → 6,919), call edges **73,053 → 73,137**, direct
**19,503 → 19,427**, external **53,441 → 53,601**, arg-flow
**17,334 → 17,245**. Indirect edges (**109**) and dlsym edges (0) are
unchanged, every checked dispatch target set is unchanged, and the C++
overload-group count rises 272 → 273. `scripts/eval_check.py` passes all 83
checks against the re-captured expectations.

**Re-verified 2026-09-05 (review of #46):** review found that the
macro-annotated conversion-operator repair above was verified against
primitive targets only, and that the other four target kinds each came out
wrong: the declarator the `ERROR` parks the target in was *walked*, which
yields the target's last segment alone, so a qualified target lost its scope
(`C::operator S` beside the `C::operator ns::S` every other spelling
produces), a template target lost its arguments (`C::operator Vec`), and a
function-pointer target lost its `(*)` and merged with the class's conversion
to the same head type (`C::operator int`). A pointer or reference target
recovers a whole `function_declarator` instead and pays elsewhere: the
member's `;` goes missing and the trailing macro is parked after it as its own
class-body `declaration`, which registered an undefined `Cls::GUARDED_BY` — the
same phantom this branch claims to have removed, in an untested corner. The
target is now read from the source text rather than walked, and a
`declaration` following a member closed by a missing `;` declares nothing.

Verifying that against the *other* target kinds turned up two more, neither
raised in review and both present on `master`: a multi-word primitive target
(`MACRO operator unsigned long() const;`) is recovered as loose keywords with
no declarator anywhere, so the member-vs-data test read it as a data field and
dropped it, and with a trailing macro to fall through to it was named
`C::operator unsigned long()const GUARDED_BY`. Both are fixed. A *globally*
qualified target behind a leading macro (`MACRO operator ::ns::S() const;`) is
not: it is the one recovery that parks its `ERROR` at class-body level rather
than inside the member, out of the member walk's reach, and repairing it means
reading recovery marks at a level that reads none today. It is recorded in
`docs/ANALYSIS.md` and pinned as the single exclusion of the new invariant
test, which asserts that all four macro spellings of fourteen target kinds name
one member and no phantom — the shape of test that would have caught the four
defects review found by hand.

Both fixes are corpus-neutral: `scripts/eval_check.py` passes **83 checks, 0
failures** against the pinned expectations, with `files`, `diagnostics`,
`edges_indirect` and `dlsym_edges` matching exactly on all three corpora and
every function/edge total inside its band. None of the corpora spells a
conversion operator with a macro on either side, which is why nothing moves —
the same reason the target-canonicalization fixes above moved nothing, and the
reason review had to reach for a scratch integration test rather than the
corpora to find these.

Review also corrected a claim in `docs/ANALYSIS.md`: `operator int(*)(char)`
and `operator int(*)(long)` were said to collide under one name because the
name keeps the `(*)` but not what follows it. The name runs to the *member's*
own parameter list, so it keeps the target's — the two are distinct members.
What collapses is the recorded *type*: `conversion_target_type` builds every
`FnPtr` with empty parameters, so both are `Ptr(FnPtr{Int, params: []})`. The
limit is real, the stated reason was not.

### Reproducing the exact numbers above

`eval_check.py` guards bands and minimums, so the per-name counts in this
section are not among its 83 checks. They come from the two corpus DBs (one
built with this tree, one with a `master` worktree binary — see "Attributing a
change: baseline vs. branch" in the Appendix) and are reproducible with:

```sql
-- 232 / 38 and their resolutions, on each DB
SELECT f.name, e.resolution, COUNT(*) FROM call_edges e
  JOIN functions f ON f.id = e.callee_fn_id
 WHERE f.name LIKE '%AssertStatus' GROUP BY 1, 2 ORDER BY 1, 2;

-- which record carries the definition (is_defined flips between the DBs)
SELECT f.name, fi.path, f.line_start, f.is_defined FROM functions f
  JOIN files fi ON fi.id = f.file_id WHERE f.name LIKE '%AssertStatus';

-- per-callee direct-edge counts; diff the two DBs' output for the -266/+190
SELECT f.name, COUNT(*) FROM call_edges e
  JOIN functions f ON f.id = e.callee_fn_id
 WHERE e.resolution = 'direct' GROUP BY 1;

-- 272 -> 273 (same SQL as the eval_check probe, which only asserts >= 240)
SELECT COUNT(*) FROM (
  SELECT name FROM functions WHERE is_defined = 1 GROUP BY name HAVING COUNT(*) > 1);

-- 27 files stop failing: diff these two lists. `stage = 'parse'` alone is
-- broader than parse failures (lower.rs raises other parse-stage
-- diagnostics), so filter the message the way examples/parse_failures.rs
-- does.
SELECT DISTINCT message FROM diagnostics
 WHERE stage = 'parse' AND message LIKE 'parse errors in %' ORDER BY 1;
```

`scripts/eval_expected.json` and `docs/PARSE_FAILURES.md` were re-captured
from this run; a lexer regression pins `...` as one token (and `..` as two),
and a preprocessor regression pins the round-trip of a variadic declaration.

**Re-verified 2026-09-04 (gMock fallback macros, #15):** the pinned Camera
corpus was re-analyzed with the current release binary and with a binary built
from `master`, so every delta below is attributed rather than assumed. Without
the gMock headers, `MOCK_METHOD` and the legacy numbered `MOCK_METHODn` /
`MOCK_CONST_METHODn` forms survived preprocessing and corrupted the mock class
containing them. Each is now recovered as the member prototype it declares:
the modern form keeps its return type, parameters and C++ qualifiers, and the
legacy forms are split at the parameter list of their single function-type
argument, so the recovered declaration spells the mocked return type and
parameters instead of a placeholder. (Prototype parameters are not lowered
into the IR today, so this shows up in the preprocessed text and in the
`cargo test` regressions rather than in the corpus metrics below.)

Parse failures drop from **91 to 73 files** (all three corpora, 304 → 286).
Camera's `missing type_identifier` category falls **22 → 2 files**; the
`gtest/HWTEST` category rises 16 → 18 because two files, once their gMock
errors were gone, reclassified to their remaining `missing ;` sites. The 18
recovered files are exactly the diagnostics delta **4,794 → 4,776**;
`preprocess`-stage diagnostics are unchanged at 4,703.

The recovered member prototypes add 114 external symbols, so functions move
**25,771 → 25,885** with defined functions unchanged at 18,964. Indirect
edges (109), arg-flow edges (17,334) and dlsym edges (0) are unchanged.
Direct edges move **19,513 → 19,503** and external edges **53,307 → 53,441**:
in each of the ten cases an unresolvable `ON_CALL(*this, Capture(_, _, _))`
argument, previously parsed as a call to whatever global of that name the
corpus happened to define, now resolves to the mock's own member (for example
`MockStreamOperator::Capture`), which is a declaration and therefore an
external edge. That is a precision gain, not a lost edge.

`Pipeline::LinkFilters`'s `LinkNext` may-target set widens **18 → 23**, adding
the five now-indexed mock overrides (`FilterMock`, `MockFilter`,
`MockNextFilter`, `MockPrevFilter`, `TestFilter`); this is the intended sound
may-analysis result. All other checked dispatch target sets are unchanged, and
`scripts/eval_check.py` passes all 68 checks. Regenerating
`docs/PARSE_FAILURES.md` also corrected stale error columns in the HDF and
Hiview sections (col 32 → 31 on `u"…"` literals); the `master` binary reports
the same columns, so those predate this change and come from the C++11
literal lexing in #14.

`scripts/eval_expected.json` and `docs/PARSE_FAILURES.md` were re-captured
from this run; the focused preprocessor and end-to-end indexing regressions
cover the fallback behavior.

**Re-verified 2026-09-02:** all three corpora are pinned to fixed upstream revisions (table in
the Appendix) and were re-analyzed fresh with the current tree (`cargo test --workspace`
green). `scripts/eval_check.py` passes all 67 checks (0 failures). The delta paragraph below
describes what this change moved compared to master's `eval_expected.json` values (not the
stale 2026-08-28 tables):
- the **`Improve cpp name lookup`** slice collapsed prototype/definition and
  phantom-bare-stub pairings, so on hiview/camera external function counts fell while direct
  edges rose, and hdf moved only slightly. All deltas below are **vs master's
  `eval_expected.json`** (captured at the same pinned revisions);
- **hiview** direct edges 5,961 → 7,652, external 21,946 → 20,259, indirect 18 → 24 (C++
  overload record split), functions 11,978 → 11,472, diagnostics unchanged at 57;
- **camera** direct edges 17,584 → 19,511, external 46,522 → 44,891, indirect 118 → 109,
  functions 26,043 → 25,771, diagnostics unchanged at 93;
- **hdf** direct edges 22,609 → 22,648, external 15,213 → 15,175, functions 12,551 → 12,557;
  indirect edges and diagnostics are unchanged at 4,621 and 191;
- **run-to-run drift**: the parallel index is nondeterministic within a small band
  (observed on camera: 25,771 ± 40 functions, 64,511 ± 240 edges across identical binaries),
  so exact counts can wiggle between runs.
- **superseded**: the figures in this block are the state **at `24e093f`** (the `Improve cpp
  name lookup` commit). The `#8` / `#13` section below re-measures against that baseline and
  moves several of them — hdf indirect edges and diagnostics become 4,643 and 182 — so the
  metric tables further down, and `eval_expected.json`, show the current run, not these.

**Re-verified 2026-09-02 (object-macro `(` classification fix, #6/#7):** all three corpora were
re-analyzed with the master (`c7c6def`) and post-fix binaries at the same corpus checkouts.
hiview and camera are identical. hdf differs only by **+6 direct call edges** to `HcsIsByteAlign`
(`hcs_blob_if.c`, `hcs_generate_tree.c`, `hcs_tree_if.c`): the `HCS_PREFIX_LENGTH` /
`HCS_BYTE_LENGTH` / `HCS_WORD_LENGTH` object macros, whose bodies start with
`(HcsIsByteAlign() ? …)`, were dropped as malformed function-like definitions and now expand.
Every hub target set, the indirect-edge count, diagnostics, and the parse-failure file sets are
unchanged, so `scripts/eval_expected.json` and the tables below are not touched by this change.

**Re-verified 2026-09-02 (file-scoped conditional fence, #8):** all three corpora were re-analyzed
at the pinned checkouts with the master (`24e093f`) and post-fix binaries. The two are
**metric-identical** — every global, every hub target set, every diagnostic count — so
`scripts/eval_expected.json` and the tables below are untouched by this change and
`scripts/eval_check.py` still passes **67/67**. That is the expected result rather than a weak
one: `trace analyze` did not export preprocessor diagnostics at the time (fixed by #20, see the
2026-09-03 block below), so the absence of a new
`unterminated #if` error was checked directly against the sources instead. Every C/C++ file in
the three checkouts was scanned (comments and string/char literals stripped, line continuations
joined) for a mismatch between its `#if`/`#ifdef`/`#ifndef` count and its `#endif` count:
**0 of 4,504 files** are unbalanced, which also rules out the stray `#elif`/`#else`/`#endif` case
the same fence covers. These corpora simply contain no instance of the bug; the fix is carried by
the fixtures and unit tests.

**Re-verified 2026-09-02 (`#` stringize and expansion-site attribution, #13):** all three corpora
were re-analyzed at the pinned checkouts with the master (`24e093f`) and post-fix binaries.
Master scores **67/67** on this machine, so every delta below is this change alone rather than
baseline drift. The preceding `#8` commit is metric-neutral, so these are also the deltas for
the branch as a whole.

*Stringize.* hdf moves, in the intended direction and only there. The `parse_failures` TSV drops
**1,095 → 615** ERROR rows and every one of the **171 `#(` sites is gone (171 → 0)**; the
failing-file set goes **185 → 176** — 9 files leave (the uniproton
`platform_{device,manager}_test.c`, the wifi `hdf_queue_test.c` /
`hdf_single_node_message_test.c`, and five `platform/common/*_test.c` unit tests) and **none
enter** — so `diagnostics` goes **191 → 182**, one `parse errors in …` warning per file.
`functions_total` −2 / `functions_external` −2 are the phantom undeclared functions
`SendSyncMessage` / `SendAsyncMessage` that the broken parse of `MSG_BREAK_IF_FUNCTION_FAILED(…)`
in `hdf_single_node_message_test.c` used to produce; those sites now parse as
`g_serviceA->SendSyncMessage(…)` member calls. camera's TSV is **byte-identical**. hiview keeps
the same 57 failing files but loses 4 ERROR rows inside one of them,
`plugins/faultlogger/.../faultlog_dump.cpp`: its `FAULTLOGGER_CMD_USAGE_INFO` is a raw string
literal (`R"(…)"`, still unsupported — a separate gap), and the `#Query` text inside it used to be
rewritten into a string literal by the old blanket `#ident` → `"ident"` path. `#` is now special
only inside a function-like macro body, per C11 6.10.3.2, so it stays verbatim; the file failed
before and fails after.

*Expansion-site LineMap attribution.* Macro-expanded tokens now carry the `(line, col)` of the
outermost invocation (`Token::origin`), so exported spans point at the invocation rather than the
`#define`, as AGENTS.md always specified. This moves the call-graph totals a lot, in one
direction, and the mechanism is worth spelling out: `merge_unit_index` deduplicates call sites on
`(file, line, col, callee)`. With definition-site coordinates every invocation of the same header
macro inside one translation unit produced the **same** key, so all but the first caller's sites
were silently merged away. With invocation coordinates they are distinct again — none of the
added edges is new analysis, each already existed in some unit's index before merge. Per corpus
(`24e093f` → this run):

| Corpus | Call edges | Direct | Indirect | External | Arg-flow | Functions | Diagnostics |
|--------|-----------:|-------:|---------:|---------:|---------:|----------:|------------:|
| hdf    | 42,450 → 72,170 | 22,654 → 37,427 | 4,621 → 4,643 | 15,175 → 30,100 | 34,057 → 63,471 | 12,557 → 12,555 | 191 → 182 |
| hiview | 27,928 → 28,075 | 7,658 → 7,693 | 24 → 24 | 20,246 → 20,358 | 9,037 → 9,107 | 11,467 → 11,467 | 57 → 57 |
| camera | 64,509 → 72,929 | 19,509 → 19,513 | 109 → 109 | 44,891 → 53,307 | 17,330 → 17,334 | 25,771 → 25,771 | 93 → 93 |

Files are unchanged on all three (1,483 / 1,428 / 1,593), as are `dlsym` edges; hdf's
`flow_graph_nodes` rises 156,612 → 156,821 from the newly parsed bodies. **Every hub target-name
set is unchanged.** The one site check that moved, `CaptureSession::AddOutput` line 1272
(**18 → 19** names), keeps all 18 `CanAddOutput` overrides byte-for-byte and gains
`__builtin_expect` — a call generated by a checking macro on that line that is now attributed to
it. `scripts/eval_expected.json` was re-captured for the metrics this change moves (hdf globals
plus the `arg_flow_rows_per_call_edge` probe, the hiview and camera edge totals, and that one
site); values it does not touch were left on upstream's baseline. `docs/PARSE_FAILURES.md` was
regenerated from this run. `cargo test --workspace` is green and `eval_check.py` is back to
**67/67 PASS** against the re-captured file.

**Re-verified 2026-09-03 (C++11 raw string literals, #14):** all three corpora were re-analyzed
at the pinned checkouts with the pre-fix (`#8`/`#13` branch head) and post-fix binaries. The
pre-fix binary scores **67/67** against the previous `eval_expected.json`, so every delta below is
this change alone. The lexer used to split `R"(a "q" b)"` into `R "(a " q " b)"`, so every raw
string literal — the JSON templates, regexes and `logPath:` fragments that hiview's tests and a
few production files are full of — spilled inner quotes and parentheses into the surrounding
code. It is now one token, re-emitted verbatim.

*hiview* is where the literals live. The `parse_failures` TSV drops **954 → 102** ERROR rows
and the failing-file set **57 → 37**: 20 files leave and **none enter** — 18 unit tests
(`faultlog_{formatter,cppcrash,database,jserror,cjerror}_test`, `sys_event_dao_test`,
`sys_event_service_ohos_test`, `sys_event_test`, `event_raw_encoded_and_decoded_test`,
`freeze_detector_unittest`, `event_export_write_test`, `event_{field_validator,logger_config_validate}_test`,
`bbox_detector_base_unit_test`, `utility_common_utils_test`, `cpp_crash_unittest`,
`syswarning_unittest`, `sys_event_store_utility_test`) plus two production files,
`faultlogger/.../faultlog_dump.cpp` (the `FAULTLOGGER_CMD_USAGE_INFO` literal the `#13` note
above called out) and `bbox_detectors/.../panic_report_recovery.cpp` (a `REGEX_FORMAT` regex).
The `)~ "`, `"file":`, `"pc":`, `"symbol":` and bare-`"` ERROR sites are all gone. Because those
20 files now parse, the corpus gains code: `diagnostics` **57 → 37**, call edges
**28,075 → 28,287** (direct 7,693 → 7,791, external 20,358 → 20,472), arg-flow
**9,107 → 9,243**, functions 11,467 → 11,464 with **7,727 → 7,772 defined** and
3,740 → 3,692 external (test bodies that used to be swallowed by a broken literal now define
their functions instead of leaving phantom externals). Indirect edges stay at **24**, `dlsym`
edges at 1, and **every hub target-name set and site check is unchanged**.

*camera* loses its two raw-string users, `camera_rotate_param_{manager,reader}_unittest.cpp`
(31 ERROR rows, **850 → 819**), so `diagnostics` goes **93 → 91**; every other camera metric is
within the usual ±1 run-to-run drift and no site check moves. *hdf* has no raw string literals;
its TSV is **byte-identical** (615 rows, 176 files) and all its metrics are unchanged.

`scripts/eval_expected.json` was re-captured for the hiview globals and the two `diagnostics`
counts; `docs/PARSE_FAILURES.md` was regenerated from this run (hiview's "generic ERROR nodes"
category 43 → 24 files). `cargo test --workspace` is green and `eval_check.py` is back to
**67/67 PASS** against the re-captured file. The follow-up that makes every string/char literal
carry its spelling — so encoding-prefixed literals (`L'x'`, `u8"s"`) are one token instead of
`L 'x'`, a tree-sitter ERROR site — was re-run the same way and is **metric-identical**: the
three corpora contain no prefixed literal outside format strings and character arrays (every
`u"` / `u'` grep hit is `%{public}u"` or `{'s','u',...}`), so it is carried by the fixture and
unit tests. A review pass on top of that (stringizing a raw string that spans lines now writes
the embedded newline as `\n`, as gcc does, instead of emitting a string literal with a bare
newline in it) was re-run the same way and is also metric-identical: no corpus file passes a
multi-line raw string through `#`. Likewise the follow-up that keeps a C++11 user-defined-literal
suffix inside its literal token (`R"(json)"_json` used to come out as `R"(json)" _json`, which is
no longer a user-defined literal): 67/67 and metric-identical, since none of the three corpora
uses a user-defined literal.

**Re-verified 2026-09-03 (preprocessor diagnostics exported, #20):** all three corpora were
re-analyzed at the pinned checkouts with the master (`9d15520`, i.e. with #14 merged) and post-fix
binaries. Master scores **67/67** against its own `eval_expected.json` on this machine. The
post-fix binary moves exactly one metric per corpus, `diagnostics`, and nothing else — every
global, every hub target set, the `parse`-stage rows (182 / 37 / 91) and the parse-failure file
sets are unchanged, so `docs/PARSE_FAILURES.md` is untouched. `trace analyze` used to drop
`PreprocessResult::diagnostics` on the floor (the index cache kept only the text, the LineMap and
the included headers); it now forwards each one as a `stage = 'preprocess'` row with the
preprocessor's severity, the originating file and line, deduplicated on `(file, line, message)`
because a header's condition is reproduced by every translation unit that expands it inline. A
header reached from both C and C++ units is warmed under both lexers but cached in one (#14);
review found the other run's diagnostics were discarded with its text, so they are now forwarded
from the warm pass too — none of the three corpora has a shared header whose report differs by
lexer, so that part moves no number here and is carried by the
`preprocess_diagnostics_survive_second_language_warm` regression test. The new rows:

| Corpus | `diagnostics` | `preprocess` rows | Of which |
|--------|--------------:|------------------:|----------|
| hdf    | 182 → 1,777 | 1,595 in 622 files | 1,592 `include file not found` (233× `securec.h`, 66× `<string>`, 63× `gtest/gtest.h`, 60× `unistd.h`, …), 3 `unknown directive` |
| hiview | 37 → 2,964 | 2,927 in 1,010 files | 2,925 `include file not found` (429× `<string>`, 189× `<memory>`, 174× `<vector>`, 143× `gtest/gtest.h`, …), 2 `unknown directive` |
| camera | 91 → 4,794 | 4,703 in 1,166 files | all `include file not found` (243× `<cstdint>`, 202× `<memory>`, 172× `gtest/gtest.h`, 141× `<mutex>`, 136× `ipc_skeleton.h`, …) |

All of it is warnings, and nearly all of it is the expected no-sysroot signal: system, libstdc++,
`securec` and gtest headers are not in the checkouts, so each `#include` of one is reported once
per including file and line. Every row has a `file_id` inside its corpus; no row is a duplicate.
Measured on the pre-#14 tree, hiview additionally showed three `error` rows
(`expected directive name after #`, each followed by a `preprocess stopped in …` warning) in
`utility_common_utils_test.cpp:232`, `cpp_crash_unittest.cpp:55` and `syswarning_unittest.cpp:57`:
a multi-line raw-string literal whose next line starts with `#04 pc …`, which the old lexer handed
to the preprocessor as a directive. With #14 merged those six rows are gone, which is the first
time that fix is visible in the `diagnostics` table rather than only in the parse counts. The
counts reproduced exactly across repeated runs of the post-fix binary, as the design intends (the
row set is fixed by the sequential warm pass plus the dedup, not by scheduling), so
`scripts/eval_expected.json` `diagnostics` values are re-captured to 1,777 / 2,964 / 4,794 and
`eval_check.py` is back to **67/67 PASS**. Every fixture directory under `tests/fixtures/` was
also analyzed with both binaries: 62 of 72 exports are identical (the `analysis_run` timestamp
excluded) and the other 10 differ only by the new `diagnostics` rows, see
`docs/INSPECT_REPORT.md`.

Performance was re-measured with the current binary (fresh runs, `--jobs 8`; stage timers
are stable, wall-clock varies with cache so values are rounded).

Each corpus is a separate section: **performance first**, then the **complete case list**. A case is file, line, function, and the full list of resolved function-pointer (or CHA virtual) targets from this binary.

C++ fixture coverage (`cpp_basic`, `cpp_dispatch`, `cpp_callable`, `cpp_flow`, …) lives under `tests/fixtures/` and is exercised by `cargo test`, not as a corpus below.

---

# 1. `drivers_hdf_core`

**Path:** `~/drivers_hdf_core`  
**Role:** OpenHarmony HDF kernel driver framework — C/C++ function-pointer dispatch  

## Performance

| Step | Time |
|------|-----:|
| Index | 3.6s |
| Analyze | 1.6s |
| Export | 0.8s |
| **Wall** | **6.2s** |

| Metric | Value |
|--------|------:|
| Files | 1,483 |
| Functions | 12,555 (10,223 defined / 2,332 external) |
| Call edges | 72,170 |
| Direct / indirect / external | 37,427 / **4,643** / 30,100 |
| Arg-flow edges | 63,471 |
| Parse warnings | 169 |
| Preprocess diagnostics | 1,595 |
| `dlsym` PAG edges | 4 |

Sequential warm, then **wave-parallel PCH** (626 headers). Nested merge is **types/typedefs** from **direct** includes plus this header's preprocess `included_headers` (child units already nested-merged grandchild types). Each TU merges **symbols** from every include-graph-reachable header plus preprocess `included_headers`. After warm, preprocess `included_headers` are added as include-graph edges so a header is never PCH'd in the same wave as a nested type the raw `#include` scanner missed; headers that become reachable only then move from the orphan path into PCH. Include-graph **cycles** are indexed in order, not as a parallel leftover wave. That was the `DeviceNodeExtDispatch` 73→72 drop (`DispatchToMessage`): `hdf_wifi_core.c` designated `.object.objectId = 1, .Dispatch = DispatchToMessage` needs a complete `struct HdfObject` prefix inside `IDeviceIoService`. Parallel leaves used to intern that nested tag empty; sequential path-sort happened to PCH `hdf_object.h` first. With preprocess edges, waves keep all 73 names (including `DispatchToMessage`). `pch-done` 0.2s vs 1.0s sequential. Index also keeps a named-tag → richest-`TypeId` map (no scan of `types[]` on every intern), shares file/preprocessed text as `Arc<str>`, caches `canonicalize`, and builds each TU's header preamble from one PCH topo order (no per-TU Kahn sort or recanonicalize of graph keys).

Hub unique-indirect counts on this corpus: `DeviceNodeExtDispatch` **74** (includes `DispatchToMessage` and new `ScanDevice`), `HdfDeviceLaunchNode` **126** (new `HdfUartInit`), `HdfSbufReadBuffer` **2**, `StreamDispatch` **24**, `HdfCameraDispatch` **23**, `HdfPmDriverDispatch` **19**, `HdfObjectManagerGetObject` **18**, `PlatformDumperDump` **13**, `SetOption` **13**, `DeviceDriverBind` 124 edges / **107** names, `GpioOnDevEventReceive` 13 edges / **12** names. Leftovers: `HdfDeviceUnlaunchNode` **113** names (new `HdfUartRelease`), linux `WorkEntry` **20**. Global indirect is **4,643**. Because same-name overloads now stay distinct, a hub can have more `functions` rows than names; the counts above are **unique names** (call/export rows may be a few higher).

## Cases

### 1. `DeviceNodeExtDispatch` — HDF device-node dispatch hub

| Field | Value |
|-------|-------|
| File | `framework/core/common/src/hdf_device_node_ext.c` |
| Line | 20–50 |
| Function | `DeviceNodeExtDispatch` |
| Function-pointer sites | `deviceMethod->Dispatch` (line 47) |
| Resolved targets | **74** |

Central device IPC dispatch: `deviceMethod->Dispatch`.

**Resolved function-pointer targets:**

- `AdcManagerDispatch`
- `AdcTestDispatch`
- `BacklightDispatch`
- `CanServiceDispatch`
- `CanTestDispatch`
- `ClockManagerDispatch`
- `ClockTestDispatch`
- `ControlDispatch`
- `DacManagerDispatch`
- `DacTestDispatch`
- `DispatchAccel`
- `DispatchAls`
- `DispatchBarometer`
- `DispatchCommand`
- `DispatchGas`
- `DispatchGravity`
- `DispatchGyro`
- `DispatchHall`
- `DispatchHumidity`
- `DispatchLight`
- `DispatchMagnetic`
- `DispatchPedometer`
- `DispatchPpg`
- `DispatchProximity`
- `DispatchSensor`
- `DispatchTemperature`
- `DispatchToMessage`
- `DispatchVibrator`
- `GpioServiceDispatch`
- `GpioTestDispatch`
- `HdfCameraDispatch`
- `HdfDispDispatch`
- `HdfEnCoderDispatch`
- `HdfHIDDispatch`
- `HdfInfraredDispatch`
- `HdfKeventIoServiceDispatch`
- `HdfKeyDispatch`
- `HdfPmDriverDispatch`
- `HdfTestCaseProcess`
- `HdfTouchDispatch`
- `HdfUeventDriverDispatch`
- `HdmiIoDispatch`
- `HelperDriverDispatch`
- `I2cTestDispatch`
- `I3cTestDispatch`
- `MmcIoDispatch`
- `PcieBusTestDispatch`
- `PcieIoDispatch`
- `PcieTestDispatch`
- `PinIoManagerDispatch`
- `PinTestDispatch`
- `PwmIoDispatch`
- `PwmTestDispatch`
- `RtcIoDispatch`
- `RtcTestDispatch`
- `SampleDispatch`
- `SampleDriverDispatch`
- `SampleServiceDispatch`
- `ScanDevice`
- `SensorTestDispatch`
- `SpiIoDispatch`
- `SpiTestDispatch`
- `StreamDispatch`
- `TestDispatch`
- `TimerIoDispatch`
- `TimerTestDispatch`
- `UartIoDispatch`
- `UartTestDispatch`
- `UsbPnpManagerDispatch`
- `UsbPnpNotifyDispatch`
- `UsbTestPnpNotifyDispatch`
- `UsbnetAdapterDispatch`
- `WatchdogIoDispatch`
- `WatchdogTestDispatch`

### 2. `HandleRequestMessage` — WiFi command dispatch table

| Field | Value |
|-------|-------|
| File | `framework/model/network/wifi/platform/src/message/nodes/local_node.c` |
| Line | 32–51 |
| Function | `HandleRequestMessage` |
| Function-pointer sites | `messageDef->handler` (line 48) |
| Resolved targets | **56** |

WiFi command table: `messageDef->handler`.

**Resolved function-pointer targets:**

- `FuncNoLoad`
- `FuncSmallLoad`
- `WifiCmdAbortScan`
- `WifiCmdAddIf`
- `WifiCmdAssoc`
- `WifiCmdCancelRemainOnChannel`
- `WifiCmdChangeBeacon`
- `WifiCmdDelKey`
- `WifiCmdDisableEapol`
- `WifiCmdDisconnect`
- `WifiCmdDoResetChip`
- `WifiCmdEnableEapol`
- `WifiCmdGetAddr`
- `WifiCmdGetApBandwidth`
- `WifiCmdGetAssociatedStas`
- `WifiCmdGetChipId`
- `WifiCmdGetDevMacAddr`
- `WifiCmdGetDriverFlag`
- `WifiCmdGetHwFeature`
- `WifiCmdGetIfNamesByChipId`
- `WifiCmdGetNetDevInfo`
- `WifiCmdGetNetworkInfo`
- `WifiCmdGetPowerMode`
- `WifiCmdGetSignalPollInfo`
- `WifiCmdGetSupportCombo`
- `WifiCmdGetValidFreqsWithBand`
- `WifiCmdIsSupportCombo`
- `WifiCmdNewKey`
- `WifiCmdProbeReqReport`
- `WifiCmdReceiveEapol`
- `WifiCmdRemainOnChannel`
- `WifiCmdRemoveIf`
- `WifiCmdResetDriver`
- `WifiCmdScan`
- `WifiCmdSendAction`
- `WifiCmdSendEapol`
- `WifiCmdSetAp`
- `WifiCmdSetApWpsP2pIe`
- `WifiCmdSetClient`
- `WifiCmdSetCountryCode`
- `WifiCmdSetKey`
- `WifiCmdSetMacAddr`
- `WifiCmdSetMode`
- `WifiCmdSetNetdev`
- `WifiCmdSetPowerMode`
- `WifiCmdSetScanningMacAddress`
- `WifiCmdSetTxPower`
- `WifiCmdStaRemove`
- `WifiCmdStartChannelMeas`
- `WifiCmdStartPnoScan`
- `WifiCmdStopAp`
- `WifiCmdStopPnoScan`
- `WifiGetStationInfo`
- `WifiSendCmdIoctl`
- `WifiSendMlme`
- `WifiSetProjectionScreenParam`

### 3. `HdfDeviceLaunchNode` — Driver initialization

| Field | Value |
|-------|-------|
| File | `framework/core/host/src/hdf_device_node.c` |
| Line | 94–131 |
| Function | `HdfDeviceLaunchNode` |
| Function-pointer sites | `driverEntry->Init` (line 116) |
| Resolved targets | **126** |

Driver init table: `driverEntry->Init`.

**Resolved function-pointer targets:**

- `AccelInitDriver`
- `AdcManagerInit`
- `AdcTestInit`
- `AlsInitDriver`
- `AudioControlInit`
- `AudioDriverInit`
- `AudioHdmiCodecDriverInit`
- `AudioStreamInit`
- `AudioUsbCodecDriverInit`
- `AudioUsbDmaDriverInit`
- `BacklightInit`
- `BarometerInitDriver`
- `BlPwmEntryInit`
- `CanTestInit`
- `ClockManagerInit`
- `ClockTestInit`
- `DacManagerInit`
- `DacTestInit`
- `DummyI2cInit`
- `EdtFocalChipInit`
- `EmmcTestInit`
- `GasInitDriver`
- `GpioDriverInit`
- `GpioServiceInit`
- `GpioTestInit`
- `GravityInitDriver`
- `GyroInitDriver`
- `HallInitDriver`
- `HdfCameraDriverInit`
- `HdfDispEntryInit`
- `HdfDrmPanelEntryInit`
- `HdfEnCoderDriverInit`
- `HdfEthDriverInit`
- `HdfFocalChipInit`
- `HdfGoodixChipInit`
- `HdfHIDDriverInit`
- `HdfHelperDriverInit`
- `HdfInfraredDriverInit`
- `HdfInputManagerInit`
- `HdfKeventDriverInit`
- `HdfKeyDriverInit`
- `HdfPmDriverInit`
- `HdfPwmInit`
- `HdfSample1DriverInit`
- `HdfSampleDriverInit`
- `HdfSoftbusDriverInit`
- `HdfSpiDeviceInit`
- `HdfTestDriverInit`
- `HdfTouchDriverProbe`
- `HdfUartDeviceInit`
- `HdfUartInit`
- `HdfUeventDriverInit`
- `HdfVirtualCanInit`
- `HdfWdtInit`
- `HdfWlanMainInit`
- `HdmiTestInit`
- `Hi35xxEntryInit`
- `Hi35xxMipiTxInit`
- `HiRtcInit`
- `HumidityInitDriver`
- `I2cDriverInit`
- `I2cManagerInit`
- `I2cTestInit`
- `I2sTestInit`
- `I3cManagerInit`
- `I3cTestInit`
- `Icn9700EntryInit`
- `Ili9881cBoeEntryInit`
- `InitLightDriver`
- `InitSensorDevManager`
- `InitSensorDriverTest`
- `InitVibratorDriver`
- `LcdkitEntryInit`
- `LinuxAdcInit`
- `LinuxClockInit`
- `LinuxEmmcInit`
- `LinuxGpioInit`
- `LinuxI2cInit`
- `LinuxRegulatorInit`
- `LinuxSdioInit`
- `MagneticInitDriver`
- `MipiCsiAdapterInit`
- `MipiCsiTestInit`
- `MipiDsiAdapterInit`
- `MipiDsiTestInit`
- `PanelEntryInit`
- `PcieBusTestInit`
- `PcieTestInit`
- `PcieVirtualAdapterInit`
- `PedometerInitDriver`
- `PinTestInit`
- `PlatformTestInit`
- `PpgInitDriver`
- `ProximityInitDriver`
- `PwmDriverInit`
- `PwmTestInit`
- `RegulatorManagerInit`
- `RegulatorTestInit`
- `RtcTestInit`
- `SampleUartDriverInit`
- `SdioTestInit`
- `SpiDriverInit`
- `SpiTestInit`
- `SspSt7789EntryInit`
- `TemperatureInitDriver`
- `TimerManagerInit`
- `TimerTestInit`
- `UartDriverInit`
- `UartTestInit`
- `UsbPnpManagerInit`
- `UsbPnpNotifyInit`
- `UsbTestPnpNotifyInit`
- `UsbnetAdapterInit`
- `VirtualAdcInit`
- `VirtualClockInit`
- `VirtualDacInit`
- `VirtualI3cInit`
- `VirtualPinInit`
- `VirtualPwmInit`
- `VirtualRegulatorInit`
- `VirtualSpiDeviceInit`
- `VirtualWatchdogInit`
- `WatchdogDriverInit`
- `WatchdogTestInit`
- `i2cDriverInit`
- `pinManagerInit`

### 4. `StreamDispatch` — Audio stream command dispatch

| Field | Value |
|-------|-------|
| File | `framework/model/audio/dispatch/src/audio_stream_dispatch.c` |
| Line | 1602–1614 |
| Function | `StreamDispatch` |
| Function-pointer sites | `g_streamDispCmdHandle[i]->func` (line 1609) |
| Resolved targets | **24** |

Audio stream command table `g_streamDispCmdHandle[i]->func`.

**Resolved function-pointer targets:**

- `StreamHostCaptureClose`
- `StreamHostCaptureOpen`
- `StreamHostCapturePause`
- `StreamHostCapturePrepare`
- `StreamHostCaptureResume`
- `StreamHostCaptureStart`
- `StreamHostCaptureStop`
- `StreamHostDspDecode`
- `StreamHostDspEncode`
- `StreamHostDspEqualizer`
- `StreamHostHwParams`
- `StreamHostMmapPositionRead`
- `StreamHostMmapPositionWrite`
- `StreamHostMmapRead`
- `StreamHostMmapWrite`
- `StreamHostRead`
- `StreamHostRenderClose`
- `StreamHostRenderOpen`
- `StreamHostRenderPause`
- `StreamHostRenderPrepare`
- `StreamHostRenderResume`
- `StreamHostRenderStart`
- `StreamHostRenderStop`
- `StreamHostWrite`

### 5. `BacklightDispatch` — Display brightness dispatch

| Field | Value |
|-------|-------|
| File | `framework/model/display/driver/backlight/hdf_bl.c` |
| Line | 398–412 |
| Function | `BacklightDispatch` |
| Function-pointer sites | `blCmdHandle` (line 411) |
| Resolved targets | **6** |

Backlight command table `blCmdHandle`.

**Resolved function-pointer targets:**

- `HdfGetBlDevList`
- `HdfGetCurrBrightness`
- `HdfGetDefBrightness`
- `HdfGetMaxBrightness`
- `HdfGetMinBrightness`
- `HdfSetBrightness`

### 6. `ControlDispatch` — Audio control dispatch

| Field | Value |
|-------|-------|
| File | `framework/model/audio/dispatch/src/audio_control_dispatch.c` |
| Line | 549–574 |
| Function | `ControlDispatch` |
| Function-pointer sites | `g_controlDispCmdHandle[i]->func` (line 570) |
| Resolved targets | **6** |

Audio control table `g_controlDispCmdHandle[i]->func`.

**Resolved function-pointer targets:**

- `ControlHostElemGetCard`
- `ControlHostElemInfo`
- `ControlHostElemList`
- `ControlHostElemRead`
- `ControlHostElemUnloadCard`
- `ControlHostElemWrite`

### 7. `RunDispatcher` — WiFi message dispatcher loop

| Field | Value |
|-------|-------|
| File | `framework/model/network/wifi/platform/src/message/message_dispatcher.c` |
| Line | 238–282 |
| Function | `RunDispatcher` |
| Function-pointer sites | `dispatcher->Ref` (line 253); `dispatcher->Disref` (line 258); `dispatcher->Disref` (line 276) |
| Resolved targets | **2** |

WiFi dispatcher loop; function-pointer deref of queued handlers.

**Resolved function-pointer targets:**

- `DisreferenceMessageDispatcher`
- `ReferenceMessageDispatcher`

### 8. `FinishEvent` — System event dispatcher

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/osal/src/osal_sysevent.c` |
| Line | 61–81 |
| Function | `FinishEvent` |
| Function-pointer sites | `service->dispatcher->Dispatch` (line 74) |
| Resolved targets | **5** |

Sys-event finish → registered dispatchers.

**Resolved function-pointer targets:**

- `DeviceManagerDispatch`
- `DeviceNodeExtDispatch`
- `DeviceSvcMgrDispatch`
- `HdfKIoServiceDispatch`
- `HdfSyscallAdapterDispatch`

### 9. `AdcOpen` — ADC open (user-space IPC)

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/adc/adc_if_u.c` |
| Line | 30–77 |
| Function | `AdcOpen` |
| Function-pointer sites | `service->dispatcher->Dispatch` (line 60) |
| Resolved targets | **5** |

User-space ADC open; indirect through service `Dispatch`.

**Resolved function-pointer targets:**

- `DeviceManagerDispatch`
- `DeviceNodeExtDispatch`
- `DeviceSvcMgrDispatch`
- `HdfKIoServiceDispatch`
- `HdfSyscallAdapterDispatch`

### 10. `AdcRead` — ADC read (user-space IPC)

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/adc/adc_if_u.c` |
| Line | 110–163 |
| Function | `AdcRead` |
| Function-pointer sites | `service->dispatcher->Dispatch` (line 146) |
| Resolved targets | **5** |

User-space ADC read; indirect through service `Dispatch`.

**Resolved function-pointer targets:**

- `DeviceManagerDispatch`
- `DeviceNodeExtDispatch`
- `DeviceSvcMgrDispatch`
- `HdfKIoServiceDispatch`
- `HdfSyscallAdapterDispatch`

### 11. `AdcClose` — ADC close (user-space IPC)

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/adc/adc_if_u.c` |
| Line | 79–108 |
| Function | `AdcClose` |
| Function-pointer sites | `service->dispatcher->Dispatch` (line 103) |
| Resolved targets | **5** |

User-space ADC close; indirect through service `Dispatch`.

**Resolved function-pointer targets:**

- `DeviceManagerDispatch`
- `DeviceNodeExtDispatch`
- `DeviceSvcMgrDispatch`
- `HdfKIoServiceDispatch`
- `HdfSyscallAdapterDispatch`

### 12. `AdcDeviceRead` — ADC core read

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/adc/adc_core.c` |
| Line | 306–333 |
| Function | `AdcDeviceRead` |
| Function-pointer sites | `device->ops->read` (line 330) |
| Resolved targets | **2** |

Driver-core ADC read: `device->ops->read`.

**Resolved function-pointer targets:**

- `AdcIioRead`
- `VirtualAdcRead`

### 13. `DeviceManagerDispatch` — Device manager dispatch

| Field | Value |
|-------|-------|
| File | `framework/core/common/src/devmgr_service_start.c` |
| Line | 66–106 |
| Function | `DeviceManagerDispatch` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Device-manager dispatch hub (direct calls only).

**Resolved function-pointer targets:** none.

### 14. `DevSvcManagerCreate` — Singleton service manager

| Field | Value |
|-------|-------|
| File | `framework/core/manager/src/devsvc_manager.c` |
| Line | 412–423 |
| Function | `DevSvcManagerCreate` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Singleton service-manager creation.

**Resolved function-pointer targets:** none.

### 15. `DevSvcManagerClntGetInstance` — Client singleton

| Field | Value |
|-------|-------|
| File | `framework/core/host/src/devsvc_manager_clnt.c` |
| Line | 146–155 |
| Function | `DevSvcManagerClntGetInstance` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Client singleton getter.

**Resolved function-pointer targets:** none.

### 16. `DevMgrUeventRuleCfgList` — Static uevent config list

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/manager/src/devmgr_uevent.c` |
| Line | 69–80 |
| Function | `DevMgrUeventRuleCfgList` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Static uevent config list.

**Resolved function-pointer targets:** none.

### 17. `DevSvcManagerExtStart` — Extended service manager start

| Field | Value |
|-------|-------|
| File | `framework/core/manager/src/devsvc_manager_ext.c` |
| Line | 129–165 |
| Function | `DevSvcManagerExtStart` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Extended service-manager start.

**Resolved function-pointer targets:** none.

### 18. `DevHostServiceStubDispatch` — Host service stub dispatch

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/host/src/devhost_service_stub.c` |
| Line | 80–111 |
| Function | `DevHostServiceStubDispatch` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Host-service stub IPC dispatch (direct).

**Resolved function-pointer targets:** none.

### 19. `DevHostServiceStubCreate` — Stub factory

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/host/src/devhost_service_stub.c` |
| Line | 123–135 |
| Function | `DevHostServiceStubCreate` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Stub factory.

**Resolved function-pointer targets:** none.

### 20. `DevHostServiceStubConstruct` — Stub construct

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/host/src/devhost_service_stub.c` |
| Line | 113–121 |
| Function | `DevHostServiceStubConstruct` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Stub construct.

**Resolved function-pointer targets:** none.

### 21. `DevHostServiceFullConstruct` — Full service constructor

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/host/src/devhost_service_full.c` |
| Line | 202–213 |
| Function | `DevHostServiceFullConstruct` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Full host-service constructor.

**Resolved function-pointer targets:** none.

### 22. `DevHostServiceFullDispatchMessage` — Message dispatch

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/host/src/devhost_service_full.c` |
| Line | 27–57 |
| Function | `DevHostServiceFullDispatchMessage` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Host-service message dispatch (direct).

**Resolved function-pointer targets:** none.

### 23. `GpioSetIrq` — GPIO IRQ configuration

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/gpio/gpio_if_u.c` |
| Line | 261–314 |
| Function | `GpioSetIrq` |
| Function-pointer sites | `service->dispatcher->Dispatch` (line 304) |
| Resolved targets | **5** |

GPIO IRQ configuration; userspace body calls `GpioRegListener`.

**Resolved function-pointer targets:**

- `DeviceManagerDispatch`
- `DeviceNodeExtDispatch`
- `DeviceSvcMgrDispatch`
- `HdfKIoServiceDispatch`
- `HdfSyscallAdapterDispatch`

### 24. `GetUartDeviceResource` — HCS config (uart_bes)

| Field | Value |
|-------|-------|
| File | `adapter/platform/uart/uart_bes.c` |
| Line | 510–564 |
| Function | `GetUartDeviceResource` |
| Function-pointer sites | `dri->GetUint32` (line 530); `dri->GetUint32` (line 534); `dri->GetUint32` (line 538); `dri->GetUint32` (line 542); `dri->GetUint32` (line 546); `dri->GetBool` (line 551); `dri->GetBool` (line 552) |
| Resolved targets | **2** |

HCS config: `dri->GetUint32` / `dri->GetBool`. This case is the `uart_bes` translation unit.

**Resolved function-pointer targets:**

- `HcsGetBool`
- `HcsGetUint32`

### 25. `GetUartDeviceResource` — HCS config (uart_stm32f4xx)

| Field | Value |
|-------|-------|
| File | `adapter/platform/uart/uart_stm32f4xx.c` |
| Line | 477–520 |
| Function | `GetUartDeviceResource` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

HCS config: `dri->GetUint32` / `dri->GetBool`. This case is the `uart_stm32` translation unit.

**Resolved function-pointer targets:** none.

### 26. `ChipDataHandle` — Touchscreen data (`fn_static`)

| Field | Value |
|-------|-------|
| File | `framework/model/input/driver/touchscreen/touch_ft5406.c` |
| Line | 115–162 |
| Function | `ChipDataHandle` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Touchscreen data path with `fn_static` (direct + `memset_s`).

**Resolved function-pointer targets:** none.

### 27. `AdcTestGetConfig` — ADC test configuration

| Field | Value |
|-------|-------|
| File | `framework/test/unittest/platform/common/adc_test.c` |
| Line | 27–79 |
| Function | `AdcTestGetConfig` |
| Function-pointer sites | `service->dispatcher->Dispatch` (line 50) |
| Resolved targets | **5** |

Test config retrieval; indirect through service `Dispatch`.

**Resolved function-pointer targets:**

- `DeviceManagerDispatch`
- `DeviceNodeExtDispatch`
- `DeviceSvcMgrDispatch`
- `HdfKIoServiceDispatch`
- `HdfSyscallAdapterDispatch`

### 28. `ClockManagerDispatch` — Clock platform dispatch

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/clock/clock_core.c` |
| Line | 762–801 |
| Function | `ClockManagerDispatch` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Clock platform dispatch (direct).

**Resolved function-pointer targets:** none.

### 29. `AudioCodecDevInit` — Audio codec device init

| Field | Value |
|-------|-------|
| File | `framework/model/audio/core/src/audio_host.c` |
| Line | 60–87 |
| Function | `AudioCodecDevInit` |
| Function-pointer sites | `codec->devData->Init` (line 78) |
| Resolved targets | **2** |

Audio codec `codec->devData->Init`.

**Resolved function-pointer targets:**

- `AudioHdmiCodecDeviceInit`
- `AudioUsbCodecDeviceInit`

### 30. `AudioDmaConfigChannel` — DMA channel configuration

| Field | Value |
|-------|-------|
| File | `framework/model/audio/common/src/audio_dma_base.c` |
| Line | 40–46 |
| Function | `AudioDmaConfigChannel` |
| Function-pointer sites | `data->ops->DmaConfigChannel` (line 43) |
| Resolved targets | **1** |

DMA config: `data->ops->DmaConfigChannel`.

**Resolved function-pointer targets:**

- `AudioUsbDmaConfigChannel`

### 31. `PlatformManagerTestAddAndDel` — Platform manager test (uniproton)

| Field | Value |
|-------|-------|
| File | `adapter/khdf/uniproton/test/sample_driver/src/platform_manager_test.c` |
| Line | 88–152 |
| Function | `PlatformManagerTestAddAndDel` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

Uniproton platform-manager test lifecycle.

**Resolved function-pointer targets:** none.

### 32. `HdfSbufReadBuffer` — C + C++ sbuf readBuffer

| Field | Value |
|-------|-------|
| File | `framework/utils/src/hdf_sbuf.c` |
| Line | 194–198 |
| Function | `HdfSbufReadBuffer` |
| Function-pointer sites | `sbuf->impl->readBuffer` (line 197) |
| Resolved targets | **2** |

C/C++ sbuf interop: `sbuf->impl->readBuffer` (FieldId guard: exactly 2).

**Resolved function-pointer targets:**

- `SbufMParcelImplReadBuffer`
- `SbufRawImplReadBuffer`

### 33. `HdfDeviceUnlaunchNode` — Driver teardown

| Field | Value |
|-------|-------|
| File | `framework/core/host/src/hdf_device_node.c` |
| Line | 183–222 |
| Function | `HdfDeviceUnlaunchNode` |
| Function-pointer sites | `driverEntry->Release` (line 200); `devNode->super->RemoveService` (line 209); `driverLoader->ReclaimDriver` (line 216) |
| Resolved targets | **113** |

Driver teardown: `driverEntry->Release`. Unique names **113**.

**Resolved function-pointer targets:**

- `AccelReleaseDriver`
- `AdcManagerRelease`
- `AdcTestRelease`
- `AlsReleaseDriver`
- `AudioControlRelease`
- `AudioDriverRelease`
- `AudioHdmiCodecDriverRelease`
- `AudioStreamRelease`
- `AudioUsbCodecDriverRelease`
- `AudioUsbDmaDriverRelease`
- `BarometerReleaseDriver`
- `CanTestRelease`
- `ClockManagerRelease`
- `ClockTestRelease`
- `DacManagerRelease`
- `DacTestRelease`
- `DummyI2cRelease`
- `EmmcTestRelease`
- `GasReleaseDriver`
- `GpioDriverRelease`
- `GpioServiceRelease`
- `GpioTestRelease`
- `GravityReleaseDriver`
- `GyroReleaseDriver`
- `HallReleaseDriver`
- `HdfCameraDriverRelease`
- `HdfDeviceNodeRemoveService`
- `HdfEncoderDriverRelease`
- `HdfEthDriverRelease`
- `HdfFocalChipRelease`
- `HdfGoodixChipRelease`
- `HdfHIDDriverRelease`
- `HdfHelperDriverRelease`
- `HdfInfraredDriverRelease`
- `HdfInputManagerRelease`
- `HdfKeventDriverRelease`
- `HdfPmDriverRelease`
- `HdfPwmRelease`
- `HdfSample1DriverRelease`
- `HdfSampleDriverRelease`
- `HdfSoftbusDriverRelease`
- `HdfSpiDeviceRelease`
- `HdfTestDriverRelease`
- `HdfTouchDriverRelease`
- `HdfUartDeviceRelease`
- `HdfUartRelease`
- `HdfUeventDriverRelease`
- `HdfVirtualCanRelease`
- `HdfWdtRelease`
- `HdfWlanDriverRelease`
- `HdmiTestRelease`
- `Hi35xxMipiTxRelease`
- `HiRtcRelease`
- `HumidityReleaseDriver`
- `I2cDriverRelease`
- `I2cManagerRelease`
- `I2cTestRelease`
- `I2sTestRelease`
- `I3cManagerRelease`
- `I3cTestRelease`
- `LinuxAdcRelease`
- `LinuxClockRelease`
- `LinuxEmmcRelease`
- `LinuxGpioRelease`
- `LinuxI2cRelease`
- `LinuxRegulatorRelease`
- `LinuxSdioRelease`
- `MagneticReleaseDriver`
- `MipiCsiAdapterRelease`
- `MipiDsiAdapterRelease`
- `PcieBusTestRelease`
- `PcieTestRelease`
- `PcieVirtualAdapterRelease`
- `PedometerReleaseDriver`
- `PinTestRelease`
- `PlatformTestRelease`
- `PpgReleaseDriver`
- `ProximityReleaseDriver`
- `PwmDriverRelease`
- `PwmTestRelease`
- `RegulatorManagerRelease`
- `RegulatorTestRelease`
- `ReleaseLightDriver`
- `ReleaseSensorDevManager`
- `ReleaseSensorDriverTest`
- `ReleaseVibratorDriver`
- `RtcTestRelease`
- `SampleUartDriverRelease`
- `SdioTestRelease`
- `SpiDriverRelease`
- `SpiTestRelease`
- `TemperatureReleaseDriver`
- `TimerManagerRelease`
- `TimerTestRelease`
- `UartDriverRelease`
- `UartTestRelease`
- `UsbPnpManagerRelease`
- `UsbPnpNotifyRelease`
- `UsbTestPnpNotifyRelease`
- `UsbnetAdapterRelease`
- `VirtualAdcRelease`
- `VirtualClockRelease`
- `VirtualDacRelease`
- `VirtualI3cRelease`
- `VirtualPinRelease`
- `VirtualPwmRelease`
- `VirtualRegulatorRelease`
- `VirtualSpiDeviceRelease`
- `VirtualWatchdogRelease`
- `WatchdogDriverRelease`
- `WatchdogTestRelease`
- `i2cDriverRelease`
- `pinManagerRelease`

### 34. `DeviceDriverBind` — Driver binding

| Field | Value |
|-------|-------|
| File | `framework/core/host/src/hdf_device_node.c` |
| Line | 65–92 |
| Function | `DeviceDriverBind` |
| Function-pointer sites | `driverEntry->Bind` (line 84) |
| Resolved targets | **107** |

Driver bind: `driverEntry->Bind`. **124** edges / **107** unique names (several edges share a callee).

**Resolved function-pointer targets:**

- `AccelBindDriver`
- `AdcManagerBind`
- `AdcTestBind`
- `AlsBindDriver`
- `AudioControlBind`
- `AudioDriverBind`
- `AudioHdmiCodecDriverBind`
- `AudioStreamBind`
- `AudioUsbCodecDriverBind`
- `AudioUsbDmaDriverBind`
- `BacklightBind`
- `BarometerBindDriver`
- `BindLightDriver`
- `BindSensorDevManager`
- `BindSensorDriverTest`
- `BindVibratorDriver`
- `CanTestBind`
- `ClockManagerBind`
- `ClockTestBind`
- `DacManagerBind`
- `DacTestBind`
- `DummyI2cBind`
- `EmmcTestBind`
- `GasBindDriver`
- `GpioDriverBind`
- `GpioServiceBind`
- `GpioTestBind`
- `GravityBindDriver`
- `GyroBindDriver`
- `HallBindDriver`
- `HdfCameraDriverBind`
- `HdfDispBind`
- `HdfEnCoderDriverBind`
- `HdfEthDriverBind`
- `HdfHIDDriverBind`
- `HdfHelperDriverBind`
- `HdfInfraredDriverBind`
- `HdfInputManagerBind`
- `HdfKeventDriverBind`
- `HdfKeyDriverBind`
- `HdfPmDriverBind`
- `HdfPwmBind`
- `HdfSample1DriverBind`
- `HdfSampleDriverBind`
- `HdfSoftbusDriverBind`
- `HdfSpiDeviceBind`
- `HdfTestDriverBind`
- `HdfTouchDriverBind`
- `HdfUartBind`
- `HdfUartDeviceBind`
- `HdfUeventDriverBind`
- `HdfVirtualCanBind`
- `HdfWdtBind`
- `HdfWifiDriverBind`
- `HdmiTestBind`
- `HiRtcBind`
- `HumidityBindDriver`
- `I2cDriverBind`
- `I2cManagerBind`
- `I2cTestBind`
- `I2sTestBind`
- `I3cManagerBind`
- `I3cTestBind`
- `LinuxEmmcBind`
- `LinuxGpioBind`
- `LinuxI2cBind`
- `LinuxRegulatorBind`
- `LinuxSdioBind`
- `MagneticBindDriver`
- `MipiCsiAdapterBind`
- `MipiCsiTestBind`
- `MipiDsiAdapterBind`
- `MipiDsiTestBind`
- `PcieBusTestBind`
- `PcieTestBind`
- `PcieVirtualAdapterBind`
- `PedometerBindDriver`
- `PinTestBind`
- `PlatformTestBind`
- `PpgBindDriver`
- `ProximityBindDriver`
- `PwmDriverBind`
- `PwmTestBind`
- `RegulatorManagerBind`
- `RegulatorTestBind`
- `RtcTestBind`
- `SampleUartDriverBind`
- `SdioTestBind`
- `SpiDriverBind`
- `SpiTestBind`
- `TemperatureBindDriver`
- `TimerManagerBind`
- `TimerTestBind`
- `UartDriverBind`
- `UartTestBind`
- `UsbPnpManagerBind`
- `UsbPnpNotifyBind`
- `UsbTestPnpNotifyBind`
- `UsbnetAdapterBind`
- `VirtualPinBind`
- `VirtualPwmBind`
- `VirtualSpiDeviceBind`
- `VirtualWatchdogBind`
- `WatchdogDriverBind`
- `WatchdogTestBind`
- `i2cDriverBind`
- `pinManagerBind`

### 35. `HdfCameraDispatch` — Camera command dispatch

| Field | Value |
|-------|-------|
| File | `framework/model/camera/dispatch/src/camera_dispatch.c` |
| Line | 521–542 |
| Function | `HdfCameraDispatch` |
| Function-pointer sites | `g_cameraCmdHandle[i]->func` (line 538) |
| Resolved targets | **23** |

Camera command table `g_cameraCmdHandle[i].func`.

**Resolved function-pointer targets:**

- `CameraCmdCloseCamera`
- `CameraCmdEnumDevice`
- `CameraCmdEnumFmt`
- `CameraCmdGetAbility`
- `CameraCmdGetConfig`
- `CameraCmdGetCrop`
- `CameraCmdGetFPS`
- `CameraCmdGetFormat`
- `CameraCmdOpenCamera`
- `CameraCmdPowerDown`
- `CameraCmdPowerUp`
- `CameraCmdQueryConfig`
- `CameraCmdQueryMemory`
- `CameraCmdQueueInit`
- `CameraCmdReqMemory`
- `CameraCmdSetConfig`
- `CameraCmdSetCrop`
- `CameraCmdSetFPS`
- `CameraCmdSetFormat`
- `CameraCmdStreamDeQueue`
- `CameraCmdStreamOff`
- `CameraCmdStreamOn`
- `CameraCmdStreamQueue`

### 36. `PowerStateChange` — Power-state dispatch (4 sites)

| Field | Value |
|-------|-------|
| File | `framework/core/host/src/power_state_token.c` |
| Line | 58–90 |
| Function | `PowerStateChange` |
| Function-pointer sites | `stateToken->listener->Suspend` (line 67); `stateToken->listener->Resume` (line 72); `stateToken->listener->DozeSuspend` (line 77); `stateToken->listener->DozeResume` (line 82) |
| Resolved targets | **16** |

PM listener vtable: Suspend / Resume / DozeSuspend / DozeResume. Four sites × four listener families (**16** unique names).

**Resolved function-pointer targets:**

- `HdfPmHdfTestDozeResume`
- `HdfPmHdfTestDozeSuspend`
- `HdfPmHdfTestResume`
- `HdfPmHdfTestSuspend`
- `HdfPmSampleDozeResume`
- `HdfPmSampleDozeSuspend`
- `HdfPmSampleResume`
- `HdfPmSampleSuspend`
- `HdfPmTestDozeResume`
- `HdfPmTestDozeSuspend`
- `HdfPmTestResume`
- `HdfPmTestSuspend`
- `HdfSampleDozeResume`
- `HdfSampleDozeSuspend`
- `HdfSampleResume`
- `HdfSampleSuspend`

### 37. `HdfObjectManagerGetObject` — Object factory dispatch

| Field | Value |
|-------|-------|
| File | `framework/core/shared/src/hdf_object_manager.c` |
| Line | 11–22 |
| Function | `HdfObjectManagerGetObject` |
| Function-pointer sites | `targetCreator->Create` (line 16) |
| Resolved targets | **18** |

Object factory: `targetCreator->Create`.

**Resolved function-pointer targets:**

- `DevHostServiceCreate`
- `DevHostServiceStubCreate`
- `DevSvcManagerCreate`
- `DevSvcManagerExtCreate`
- `DevSvcManagerProxyCreate`
- `DevSvcManagerStubCreate`
- `DeviceNodeExtCreate`
- `DeviceServiceStubCreate`
- `DeviceTokenStubCreate`
- `DevmgrServiceCreate`
- `DevmgrServiceProxyCreate`
- `DevmgrServiceStubCreate`
- `DriverInstallerCreate`
- `DriverInstallerFullCreate`
- `HdfDeviceCreate`
- `HdfDeviceTokenCreate`
- `HdfDriverLoaderCreate`
- `HdfDriverLoaderFullCreate`

### 38. `SetOption` — Sensor option dispatch

| Field | Value |
|-------|-------|
| File | `framework/model/sensor/driver/common/src/sensor_device_manager.c` |
| Line | 216–231 |
| Function | `SetOption` |
| Function-pointer sites | `deviceInfo->ops->SetOption` (line 230) |
| Resolved targets | **13** |

Sensor `deviceInfo->ops.SetOption`.

**Resolved function-pointer targets:**

- `SetAccelOption`
- `SetAlsOption`
- `SetBarometerOption`
- `SetGasOption`
- `SetGravityOption`
- `SetGyroOption`
- `SetHallOption`
- `SetHumidityOption`
- `SetMagneticOption`
- `SetPedometerOption`
- `SetPpgOption`
- `SetProximityOption`
- `SetTemperatureOption`

### 39. `GpioOnDevEventReceive` — GPIO event callback

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/fwk/platform_listener_u.c` |
| Line | 121–149 |
| Function | `GpioOnDevEventReceive` |
| Function-pointer sites | `gpio->func` (line 146) |
| Resolved targets | **12** |

GPIO event callback: `gpio->func`. **13** edges / **12** unique names.

**Resolved function-pointer targets:**

- `GpioServiceIrqFunc`
- `GpioTestIrqHandler`
- `HallNorthPolarityIrqFunc`
- `HallSouthPolarityIrqFunc`
- `InfraredIrqHandle`
- `IrqHandle`
- `KeyIrqHandle`
- `PpgIrqHandler`
- `TestCaseGpioIrqHandler1`
- `TestCaseGpioIrqHandler2`
- `TestCaseGpioIrqHandler3`
- `TestCaseGpioIrqHandler4`

### 40. `HdfPmDriverDispatch` — PM driver test dispatch

| Field | Value |
|-------|-------|
| File | `framework/test/unittest/pm/hdf_pm_driver_test.c` |
| Line | 568–587 |
| Function | `HdfPmDriverDispatch` |
| Function-pointer sites | `g_testCases[cmdId]->testFunc` (line 581) |
| Resolved targets | **19** |

PM test driver `pdr->ops->Dispatch`.

**Resolved function-pointer targets:**

- `HdfPmTestBegin`
- `HdfPmTestEnd`
- `HdfPmTestOneDriverHundred`
- `HdfPmTestOneDriverOnce`
- `HdfPmTestOneDriverTen`
- `HdfPmTestOneDriverThousand`
- `HdfPmTestOneDriverTwice`
- `HdfPmTestThreeDriverHundred`
- `HdfPmTestThreeDriverHundredWithSync`
- `HdfPmTestThreeDriverOnce`
- `HdfPmTestThreeDriverSeqHundred`
- `HdfPmTestThreeDriverTen`
- `HdfPmTestThreeDriverThousand`
- `HdfPmTestThreeDriverTwice`
- `HdfPmTestTwoDriverHundred`
- `HdfPmTestTwoDriverOnce`
- `HdfPmTestTwoDriverTen`
- `HdfPmTestTwoDriverThousand`
- `HdfPmTestTwoDriverTwice`

### 41. `WorkEntry` — Workqueue dispatch (linux)

| Field | Value |
|-------|-------|
| File | `adapter/khdf/linux/osal/src/osal_workqueue.c` |
| Line | 51–63 |
| Function | `WorkEntry` |
| Function-pointer sites | `wrapper->workFunc` (line 57) |
| Resolved targets | **20** |

Linux workqueue: `work->func`. Unique names **20** (original eval 19; extra `AlsDataWorkEntry`).

**Resolved function-pointer targets:**

- `AccelDataWorkEntry`
- `AlsDataWorkEntry`
- `BarometerDataWorkEntry`
- `EsdWorkHandler`
- `EventQueueWorkEntry`
- `GasDataWorkEntry`
- `GravityDataWorkEntry`
- `GyroDataWorkEntry`
- `HallDataWorkEntry`
- `HumidityDataWorkEntry`
- `LightWorkEntry`
- `MagneticDataWorkEntry`
- `PedometerDataWorkEntry`
- `PpgDataWorkEntry`
- `ProximityDataWorkEntry`
- `SensorTestDataWorkEntry`
- `TemperatureDataWorkEntry`
- `TestDelayWorkEntry`
- `TestWorkEntry`
- `VibratorWorkEntry`

### 42. `PlatformDumperDump` — Platform dumper dispatch

| Field | Value |
|-------|-------|
| File | `framework/support/platform/src/fwk/platform_dumper_unopen.c` |
| Line | 21–25 |
| Function | `PlatformDumperDump` |
| Function-pointer sites | `pos->printFunc` (line 460) |
| Resolved targets | **13** |

Dumper type table: `ops->func`.

**Resolved function-pointer targets:**

- `DumperPrintCharInfo`
- `DumperPrintDoubleInfo`
- `DumperPrintFloatInfo`
- `DumperPrintInt16Info`
- `DumperPrintInt32Info`
- `DumperPrintInt64Info`
- `DumperPrintInt8Info`
- `DumperPrintRegisterInfo`
- `DumperPrintStringInfo`
- `DumperPrintUint16Info`
- `DumperPrintUint32Info`
- `DumperPrintUint64Info`
- `DumperPrintUint8Info`

### 43. `LoadIpcImpl` — dlsym IPC constructor load

| Field | Value |
|-------|-------|
| File | `framework/utils/src/hdf_sbuf.c` |
| Line | 76–106 |
| Function | `LoadIpcImpl` |
| Function-pointer sites | _none_ |
| Resolved targets | **0** |

`dlsym` of `"SbufObtainIpc"` / `"SbufBindIpc"` (call remains external libc).

**Resolved function-pointer targets:** none.

### 44. `HdfSbufTypedObtainCapacity` — sbuf obtain constructor

| Field | Value |
|-------|-------|
| File | `framework/utils/src/hdf_sbuf.c` |
| Line | 378–414 |
| Function | `HdfSbufTypedObtainCapacity` |
| Function-pointer sites | `constructor->obtain` (line 405) |
| Resolved targets | **3** |

Obtain constructor vtable after `dlsym` stores.

**Resolved function-pointer targets:**

- `SbufObtainIpc`
- `SbufObtainIpcHw`
- `SbufObtainRaw`

### 45. `DeviceServiceStubDispatch` — User-space IOService dispatch

| Field | Value |
|-------|-------|
| File | `adapter/uhdf2/host/src/device_service_stub.c` |
| Line | 26–60 |
| Function | `DeviceServiceStubDispatch` |
| Function-pointer sites | `ioService->Dispatch` (line 53) |
| Resolved targets | **73** |

Same `IDeviceIoService.Dispatch` field as case 1, from the UHDF2 stub. Target set is **identical** to `DeviceNodeExtDispatch` (verified), including `DispatchToMessage`.

**Resolved function-pointer targets:** same 73 names as case 1.

### 46. `HdfKIoServiceDispatch` — Kernel vnode IOService dispatch

| Field | Value |
|-------|-------|
| File | `framework/core/adapter/vnode/src/hdf_vnode_adapter.c` |
| Line | 56–71 |
| Function | `HdfKIoServiceDispatch` |
| Function-pointer sites | `kClient->client.device->service->Dispatch` (line 70) |
| Resolved targets | **73** |

Kernel vnode path onto the same `Dispatch` field. Target set is **identical** to case 1 (verified), including `DispatchToMessage`.

**Resolved function-pointer targets:** same 73 names as case 1.

### 47. `HdfObjectManagerFreeObject` — Object-factory Release

| Field | Value |
|-------|-------|
| File | `framework/core/shared/src/hdf_object_manager.c` |
| Line | 24–35 |
| Function | `HdfObjectManagerFreeObject` |
| Function-pointer sites | `targetCreator->Release` (line 34) |
| Resolved targets | **13** |

Teardown counterpart of case 37 (`Create` has 18; five creator-table slots set `Release = NULL` and correctly contribute no target).

**Resolved function-pointer targets:**

- `DevHostServiceRelease`
- `DevHostServiceStubRelease`
- `DevSvcManagerExtRelease`
- `DevSvcManagerProxyRelease`
- `DevSvcManagerRelease`
- `DeviceNodeExtRelease`
- `DeviceServiceStubRelease`
- `DeviceTokenStubRelease`
- `DevmgrServiceProxyRelease`
- `DevmgrServiceRelease`
- `HdfDeviceRelease`
- `HdfDeviceTokenRelease`
- `HdfDriverLoaderFullRelease`

### 48. `Enable` — Sensor ops Enable

| Field | Value |
|-------|-------|
| File | `framework/model/sensor/driver/common/src/sensor_device_manager.c` |
| Line | 162–169 |
| Function | `Enable` |
| Function-pointer sites | `deviceInfo->ops.Enable` (line 168) |
| Resolved targets | **13** |

All 13 production `deviceInfo->ops.Enable = Set*Enable` stores (accel/als/barometer/gas/gravity/gyro/hall/humidity/magnetic/pedometer/ppg/proximity/temperature). `SensorEnableTest` in the unittest file writes a **different** struct and is not a source for this site.

**Resolved function-pointer targets:**

- `SetAccelEnable`
- `SetAlsEnable`
- `SetBarometerEnable`
- `SetGasEnable`
- `SetGravityEnable`
- `SetGyroEnable`
- `SetHallEnable`
- `SetHumidityEnable`
- `SetMagneticEnable`
- `SetPedometerEnable`
- `SetPpgEnable`
- `SetProximityEnable`
- `SetTemperatureEnable`

### 49. `Disable` — Sensor ops Disable

| Field | Value |
|-------|-------|
| File | `framework/model/sensor/driver/common/src/sensor_device_manager.c` |
| Line | 171–179 |
| Function | `Disable` |
| Function-pointer sites | `deviceInfo->ops.Disable` (line 178) |
| Resolved targets | **13** |

Same 13 drivers as case 48, `Set*Disable` stores. Complete vs source.

**Resolved function-pointer targets:**

- `SetAccelDisable`
- `SetAlsDisable`
- `SetBarometerDisable`
- `SetGasDisable`
- `SetGravityDisable`
- `SetGyroDisable`
- `SetHallDisable`
- `SetHumidityDisable`
- `SetMagneticDisable`
- `SetPedometerDisable`
- `SetPpgDisable`
- `SetProximityDisable`
- `SetTemperatureDisable`

---

# 2. `hiviewdfx_hiview`

**Path:** `~/hiviewdfx_hiview`  
**Role:** OpenHarmony HiView DFX plugin platform — C++ virtual dispatch + preprocessor X-macros  

## Performance

| Step | Time |
|------|-----:|
| Index | 3.4s |
| Analyze | 0.1s |
| Export | 0.5s |
| **Wall** | **4.1s** |

| Metric | Value |
|--------|------:|
| Files | 1,428 |
| Functions | 11,464 (7,772 defined / 3,692 external) |
| Call edges | 28,287 |
| Direct / indirect / external | 7,791 / **24** / 20,472 |
| Arg-flow edges | 9,243 |
| Parse warnings | 32 |
| Preprocess diagnostics | 2,927 |
| `dlsym` PAG edges | 1 |

The tree previously aborted with a preprocessor stack overflow on `PRIVATE_MESSAGE_TYPE`. Hide-set painting is what makes it finish. The **24** indirect edges include `$lambda` / JSON accessors and C++ overload record splits; production dispatch is recovered as **direct** CHA edges. On this corpus the phantom-bare-stub count fell ~500 records (~3,959 → ~3,740 external), **direct** free-function edges rose ~1,900, arg-flow edges rose with them, and garbage `externalLogJson` indirect sites disappeared — dispatch-site correctness invariants are unchanged. The edge totals above also carry the expansion-site attribution fix shipped with #13 (+147 call edges, +70 arg-flow): macro-generated call sites are keyed by their invocation, so sites that used to collide on shared `#define` coordinates now survive merge dedup. Function counts, the **24** indirect edges and the diagnostics are unaffected by it. The raw-string lexer fix (#14) then moved the parse warnings **57 → 37** and, through the 20 files that now parse, +212 call edges, +136 arg-flow edges and +45 defined functions (see the `#14` block at the top). The `...` punctuator fix (#28) took them **37 → 32** without moving any other number on this corpus.

**Indirect edge changes vs. master** (exact-match metric, both sides):
- **Lost 3** phantom `externalLogJson` targets in `CopyExternalLogsToSandBox` (these were bare-stub false positives that the qualified-prototype merge now resolves correctly as direct calls).
- **Gained 9** lambda callbacks resolved through `WriteStringWithDesignatedLength` (the qualified prototypes in `base/json` headers enabled the solver to trace function-pointer flow into lambda `$lambda` entries that were previously unreachable).
- Net: 18 → 24 (18 − 3 + 9) is a genuine improvement from prototype-based resolution, not a regression.

## Cases

### 1. `PRIVATE_MESSAGE_TYPE` — X-macro enumerator list (preprocessor)

| Field | Value |
|-------|-------|
| File | `base/include/defines.h` |
| Line | 39–70 |
| Function | `PRIVATE_MESSAGE_TYPE` |
| Dispatch site | _preprocessor; invoked from `event.h:127`_ |
| Resolved targets | **0** |

Not a call. Hide-set paints the first replacement token so the enum list expands as gcc does. Analysis of the tree completes (previously stack-overflowed). Same pattern: `PRIVATE_AUDIT_EVENT_TYPE`.

**Resolved function-pointer / virtual targets:** none.

### 2. `OHOS::HiviewDFX::Plugin::OnEventProxy` — Virtual plugin entry (CHA)

| Field | Value |
|-------|-------|
| File | `base/plugin.cpp` |
| Line | 55–83 |
| Function | `OHOS::HiviewDFX::Plugin::OnEventProxy` |
| Dispatch site | `OnEvent(dupEvent)` rewritten as implicit `this->OnEvent` (line 68) |
| Resolved targets | **23** |

**Pass.** CHA from static type `Plugin` emits **direct** edges to defined plugin `::OnEvent` overrides, including `Plugin::OnEvent` (`plugin.cpp:35`). Five other defined `::OnEvent` methods override `EventHandler`, not `Plugin`, and appear under `EventHandler::OnEventProxy` instead.

**Resolved targets:**

- `OHOS::HiviewDFX::BBoxDetectorPlugin::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample1::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample2::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample3::OnEvent`
- `OHOS::HiviewDFX::CrashValidator::OnEvent`
- `OHOS::HiviewDFX::DynamicLoadPluginExample::OnEvent`
- `OHOS::HiviewDFX::EventLogger::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample1::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample2::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample3::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample4::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample5::OnEvent`
- `OHOS::HiviewDFX::EventValidator::OnEvent`
- `OHOS::HiviewDFX::FaultDetectorManager::OnEvent`
- `OHOS::HiviewDFX::Faultlogger::OnEvent`
- `OHOS::HiviewDFX::FreezeDetectorPlugin::OnEvent`
- `OHOS::HiviewDFX::Plugin::OnEvent`
- `OHOS::HiviewDFX::PluginExample::OnEvent`
- `OHOS::HiviewDFX::PluginProxy::OnEvent`
- `OHOS::HiviewDFX::PrivacyController::OnEvent`
- `OHOS::HiviewDFX::SysEventDispatcher::OnEvent`
- `OHOS::HiviewDFX::SysEventStore::OnEvent`
- `OHOS::HiviewDFX::UsageEventReport::OnEvent`

### 3. `OHOS::HiviewDFX::PipelineEvent::OnContinue` — Pipeline pump

| Field | Value |
|-------|-------|
| File | `base/pipeline.cpp` |
| Line | 34–70 |
| Function | `OHOS::HiviewDFX::PipelineEvent::OnContinue` |
| Dispatch site | `pluginPtr->OnEventProxy` (after `auto pluginPtr = wp.lock()`) |
| Resolved targets | **0** |

**Fail** on the plugin dispatch: `auto` / `lock()` drops the `Plugin` type, so the site has 0 targets. Unqualified recursive `OnContinue()` **does** bind (direct).

**Resolved function-pointer / virtual targets:** none.

### 4. `OHOS::HiviewDFX::PluginFactory::GetPlugin` — Constructor registry

| Field | Value |
|-------|-------|
| File | `base/plugin_factory.cpp` |
| Line | 40–47 |
| Function | `OHOS::HiviewDFX::PluginFactory::GetPlugin` |
| Dispatch site | `info->getPluginObject()` (`std::function` field) |
| Resolved targets | **0** |

Unqualified `GetGlobalPluginInfo` binds (**Pass**). `getPluginObject` has **0** targets: constructors are registered through `std::map`, so no function address reaches this load.

**Resolved function-pointer / virtual targets:** none.

### 5. `OHOS::HiviewDFX::EventLogger::OnEvent` — Plugin body (same-class directs)

| Field | Value |
|-------|-------|
| File | `plugins/eventlogger/event_logger.cpp` |
| Line | 209–209 |
| Function | `OHOS::HiviewDFX::EventLogger::OnEvent` |
| Dispatch site | _no function-pointer site_ |
| Resolved targets | **0** |

**Pass** for same-class / event API directs (`IsValidEventParam`, `GetEventPid`, `UpdateDB`, …). STL / SDK / `Event::DownCastTo` / `ffrt::submit` remain external. No function-pointer dispatch.

**Resolved function-pointer / virtual targets:** none.

### 6. `OHOS::HiviewDFX::SysEventStore::OnEvent` — Event store plugin

| Field | Value |
|-------|-------|
| File | `plugins/event_store/sys_event_store.cpp` |
| Line | 123–160 |
| Function | `OHOS::HiviewDFX::SysEventStore::OnEvent` |
| Dispatch site | _no function-pointer site_ |
| Resolved targets | **0** |

Same-class calls bind. Nested `EventStore::…`, `TriggerExportEngine`, `TimeUtil`, `Parameter::*` stay external. No function-pointer dispatch.

**Resolved function-pointer / virtual targets:** none.

### 7. `inspect calls --from OnEventProxy` — inspect suffix match

| Field | Value |
|-------|-------|
| File | `base/plugin.cpp` |
| Line | 55–83 |
| Function | `inspect calls --from OnEventProxy` |
| Dispatch site | _CLI, not a call site_ |
| Resolved targets | **0** |

**Pass.** Suffix match lists `Plugin::OnEventProxy` and `EventHandler::OnEventProxy`. `--from Get_lugin` is empty (`LIKE` `_` escaped).

**Resolved function-pointer / virtual targets:** none.

### 8. `OHOS::HiviewDFX::PluginProxy::OnEvent` — Smart-pointer field receiver

| Field | Value |
|-------|-------|
| File | `base/plugin_proxy.cpp` |
| Line | 22–30 |
| Function | `OHOS::HiviewDFX::PluginProxy::OnEvent` |
| Dispatch site | `plugin_->OnEvent(event)` (line 28), field `shared_ptr<Plugin> plugin_` |
| Resolved targets | **23** |

**Pass.** Same CHA fan-out as case 2. Fixture: `cpp_smart_ptr_field_receiver_unwraps`.

**Resolved targets:**

- `OHOS::HiviewDFX::BBoxDetectorPlugin::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample1::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample2::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample3::OnEvent`
- `OHOS::HiviewDFX::CrashValidator::OnEvent`
- `OHOS::HiviewDFX::DynamicLoadPluginExample::OnEvent`
- `OHOS::HiviewDFX::EventLogger::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample1::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample2::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample3::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample4::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample5::OnEvent`
- `OHOS::HiviewDFX::EventValidator::OnEvent`
- `OHOS::HiviewDFX::FaultDetectorManager::OnEvent`
- `OHOS::HiviewDFX::Faultlogger::OnEvent`
- `OHOS::HiviewDFX::FreezeDetectorPlugin::OnEvent`
- `OHOS::HiviewDFX::Plugin::OnEvent`
- `OHOS::HiviewDFX::PluginExample::OnEvent`
- `OHOS::HiviewDFX::PluginProxy::OnEvent`
- `OHOS::HiviewDFX::PrivacyController::OnEvent`
- `OHOS::HiviewDFX::SysEventDispatcher::OnEvent`
- `OHOS::HiviewDFX::SysEventStore::OnEvent`
- `OHOS::HiviewDFX::UsageEventReport::OnEvent`

### 9. `OHOS::HiviewDFX::Plugin::DelayProcessEvent` — `std::bind` onto the work loop

| Field | Value |
|-------|-------|
| File | `base/plugin.cpp` |
| Line | 85–96 |
| Function | `OHOS::HiviewDFX::Plugin::DelayProcessEvent` |
| Dispatch site | `std::bind(&Plugin::OnEventProxy, this, event)` (line 93) |
| Resolved targets | **0** |

**Fail.** `std::bind` is external; no edge to `OnEventProxy`. `AddTimerEvent` is direct (`EventLoop` / `MockEventLoop`).

**Resolved function-pointer / virtual targets:** none.

### 10. `OHOS::HiviewDFX::EventLoop::ProcessEvent` — Packed vs typed handler

| Field | Value |
|-------|-------|
| File | `base/event_loop.cpp` |
| Line | 492–510 |
| Function | `OHOS::HiviewDFX::EventLoop::ProcessEvent` |
| Dispatch site | `event.handler->OnEventProxy` (line 498); `event->task` (496); `event->packagedTask` (504) |
| Resolved targets | **2** |

**Partial.** Typed handler CHA **Pass** (targets below). `event->task()` and `packagedTask` have **0** targets.

**Resolved targets:**

- `OHOS::HiviewDFX::EventHandler::OnEventProxy`
- `OHOS::HiviewDFX::Plugin::OnEventProxy`

### 11. `OHOS::HiviewDFX::Event::DownCastTo` — Template `DownCastTo<SysEvent>`

| Field | Value |
|-------|-------|
| File | `base/include/event.h` |
| Line | 201–205 |
| Function | `OHOS::HiviewDFX::Event::DownCastTo` |
| Dispatch site | 13 call sites (all external `Event::DownCastTo`) |
| Resolved targets | **0** |

**Fail.** Name-stripping does not instantiate the template, so the result is not typed as `SysEvent`.

**Resolved function-pointer / virtual targets:** none.

### 12. `ffrt::submit` — `ffrt::submit` deferred lambdas

| Field | Value |
|-------|-------|
| File | `plugins/ (e.g. passthrough_monitor.cpp:80)` |
| Line | 80–80 |
| Function | `ffrt::submit` |
| Dispatch site | 34 `ffrt::submit` sites (all external) |
| Resolved targets | **0** |

**Fail.** 357 `$lambda` functions exist; 7 have in-edges, none from `ffrt::submit`.

**Resolved function-pointer / virtual targets:** none.

### 13. `OHOS::HiviewDFX::UCollectUtil::GraphicMemoryCollectorImpl::GetGraphicUsage` — `dlopen` / `dlsym`

| Field | Value |
|-------|-------|
| File | `plugins/unified_collector/graphic_memory_collector_impl.cpp` |
| Line | 47–59 |
| Function | `OHOS::HiviewDFX::UCollectUtil::GraphicMemoryCollectorImpl::GetGraphicUsage` |
| Dispatch site | `dlsym(handler, GET_INSTANCE)` with name `"GetInstance"` |
| Resolved targets | **0** |

**Fail** for in-tree callees. The `dlsym` model is wired (1 PAG `dlsym` edge) but exact-name lookup is `"GetInstance"` while the export is stored as `OHOS::HiviewDFX::UCollectUtil::GetInstance`. `CallDllFunc` / `GetSymbol` pass `std::string::c_str()`, not a folded constant.

**Resolved function-pointer / virtual targets:** none.

### 14. `OHOS::HiviewDFX::Plugin::OnEvent` — Out-of-line `Plugin::OnEvent` body

| Field | Value |
|-------|-------|
| File | `base/plugin.cpp` |
| Line | 35–38 |
| Function | `OHOS::HiviewDFX::Plugin::OnEvent` |
| Dispatch site | _definition presence, not a dispatch site_ |
| Resolved targets | **0** |

**Pass.** `is_defined=1`. Predefined empty `__UNUSED` keeps the body. Participates in cases 2 and 8.

**Resolved function-pointer / virtual targets:** none.

### 15. `OHOS::HiviewDFX::EventHandler::OnEventProxy` — EventHandler CHA

| Field | Value |
|-------|-------|
| File | `base/include/event.h` |
| Line | 230–233 |
| Function | `OHOS::HiviewDFX::EventHandler::OnEventProxy` |
| Dispatch site | `OnEvent(event)` (line 232) |
| Resolved targets | **27** |

CHA from static type `EventHandler`. The 23 plugin `::OnEvent` names from case 2 plus four handlers that override `EventHandler` but not `Plugin`: `EventHandler::OnEvent`, `OverheadCalculateEventHandler`, `RealEventHandler`, `TestEventHandler`. Complete vs defined `::OnEvent` bodies under those two bases.

**Resolved targets:**

- `OHOS::HiviewDFX::BBoxDetectorPlugin::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample1::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample2::OnEvent`
- `OHOS::HiviewDFX::BundlePluginExample3::OnEvent`
- `OHOS::HiviewDFX::CrashValidator::OnEvent`
- `OHOS::HiviewDFX::DynamicLoadPluginExample::OnEvent`
- `OHOS::HiviewDFX::EventHandler::OnEvent`
- `OHOS::HiviewDFX::EventLogger::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample1::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample2::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample3::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample4::OnEvent`
- `OHOS::HiviewDFX::EventProcessorExample5::OnEvent`
- `OHOS::HiviewDFX::EventValidator::OnEvent`
- `OHOS::HiviewDFX::FaultDetectorManager::OnEvent`
- `OHOS::HiviewDFX::Faultlogger::OnEvent`
- `OHOS::HiviewDFX::FreezeDetectorPlugin::OnEvent`
- `OHOS::HiviewDFX::OverheadCalculateEventHandler::OnEvent`
- `OHOS::HiviewDFX::Plugin::OnEvent`
- `OHOS::HiviewDFX::PluginExample::OnEvent`
- `OHOS::HiviewDFX::PluginProxy::OnEvent`
- `OHOS::HiviewDFX::PrivacyController::OnEvent`
- `OHOS::HiviewDFX::RealEventHandler::OnEvent`
- `OHOS::HiviewDFX::SysEventDispatcher::OnEvent`
- `OHOS::HiviewDFX::SysEventStore::OnEvent`
- `OHOS::HiviewDFX::TestEventHandler::OnEvent`
- `OHOS::HiviewDFX::UsageEventReport::OnEvent`

### 16. `OHOS::HiviewDFX::PluginProxy::CanProcessEvent` — Field receiver CHA

| Field | Value |
|-------|-------|
| File | `base/plugin_proxy.cpp` |
| Line | 33–40 |
| Function | `OHOS::HiviewDFX::PluginProxy::CanProcessEvent` |
| Dispatch site | `plugin_->CanProcessEvent(event)` (line 39) |
| Resolved targets | **12** |

All 11 `Plugin::CanProcessEvent` overrides in tree plus the proxy itself. `Pipeline::CanProcessEvent` takes `PipelineEvent` and is a different function. Complete vs source.

**Resolved targets:**

- `OHOS::HiviewDFX::BundlePluginExample1::CanProcessEvent`
- `OHOS::HiviewDFX::BundlePluginExample2::CanProcessEvent`
- `OHOS::HiviewDFX::BundlePluginExample3::CanProcessEvent`
- `OHOS::HiviewDFX::EventProcessorExample1::CanProcessEvent`
- `OHOS::HiviewDFX::EventProcessorExample2::CanProcessEvent`
- `OHOS::HiviewDFX::EventProcessorExample3::CanProcessEvent`
- `OHOS::HiviewDFX::EventProcessorExample4::CanProcessEvent`
- `OHOS::HiviewDFX::EventProcessorExample5::CanProcessEvent`
- `OHOS::HiviewDFX::Faultlogger::CanProcessEvent`
- `OHOS::HiviewDFX::FreezeDetectorPlugin::CanProcessEvent`
- `OHOS::HiviewDFX::Plugin::CanProcessEvent`
- `OHOS::HiviewDFX::PluginProxy::CanProcessEvent`

### 17. `OHOS::HiviewDFX::PluginProxy::OnEventListeningCallback` — Listener CHA

| Field | Value |
|-------|-------|
| File | `base/plugin_proxy.cpp` |
| Line | 75–83 |
| Function | `OHOS::HiviewDFX::PluginProxy::OnEventListeningCallback` |
| Dispatch site | `plugin_->OnEventListeningCallback(msg)` (line 81) |
| Resolved targets | **8** |

**Complete for this analysis config.** Eight overrides remain after preprocess. `UnifiedCollector::OnEventListeningCallback` exists in source (`unified_collector.h:36`, `unified_collector.cpp:201`) but only under `#ifdef UNIFIED_COLLECTOR_TRACE_ENABLE`. GN sets that define when `hiview_unified_collector_trace_enable` is on (`plugins/unified_collector/BUILD.gn`). `trace analyze` does not pass OpenHarmony product flags, so the preprocessor strips both the declaration and the body. CHA then only sees `Plugin`'s empty default in `plugin.h`. Not a CHA miss: this run is the trace-disabled variant of the plugin. Passing `-D UNIFIED_COLLECTOR_TRACE_ENABLE` would include the override.

**Resolved targets:**

- `OHOS::HiviewDFX::BundlePluginExample3::OnEventListeningCallback`
- `OHOS::HiviewDFX::EventProcessorExample4::OnEventListeningCallback`
- `OHOS::HiviewDFX::FaultDetectorManager::OnEventListeningCallback`
- `OHOS::HiviewDFX::Faultlogger::OnEventListeningCallback`
- `OHOS::HiviewDFX::FreezeDetectorPlugin::OnEventListeningCallback`
- `OHOS::HiviewDFX::Plugin::OnEventListeningCallback`
- `OHOS::HiviewDFX::PluginProxy::OnEventListeningCallback`
- `OHOS::HiviewDFX::XperfPlugin::OnEventListeningCallback`

### 18. `OHOS::HiviewDFX::EventLoop::ProcessEvent` — Handler OnEventProxy

| Field | Value |
|-------|-------|
| File | `base/event_loop.cpp` |
| Line | 492–511 |
| Function | `OHOS::HiviewDFX::EventLoop::ProcessEvent` |
| Dispatch site | `event.handler->OnEventProxy(event.event)` (line 498) |
| Resolved targets | **2** |

Typed `EventHandler*` receiver CHA to the two `OnEventProxy` implementations (`EventHandler` inline, `Plugin` override). Fan-out from those proxies to `::OnEvent` is cases 2 and 15. (`event.task()` / packaged tasks remain unresolved — case 10.)

**Resolved targets:**

- `OHOS::HiviewDFX::EventHandler::OnEventProxy`
- `OHOS::HiviewDFX::Plugin::OnEventProxy`

### 19. `OHOS::HiviewDFX::PluginProxy::GetHandlerInfo` — Field receiver

| Field | Value |
|-------|-------|
| File | `base/plugin_proxy.cpp` |
| Line | 55–65 |
| Function | `OHOS::HiviewDFX::PluginProxy::GetHandlerInfo` |
| Dispatch site | `plugin_->GetHandlerInfo()` (line 62) |
| Resolved targets | **2** |

Only `Plugin` and `PluginProxy` define `GetHandlerInfo` in this tree. Complete vs source.

**Resolved targets:**

- `OHOS::HiviewDFX::Plugin::GetHandlerInfo`
- `OHOS::HiviewDFX::PluginProxy::GetHandlerInfo`

---

# 3. Camera and clang/test

Hang / stack-overflow checks, not dispatch-hub evals. PCH-style header IR is what lets these trees finish: camera previously hung in preprocess (diamond includes); `clang/test/Sema/deep_recursion.c` overflowed a rayon worker (now 16 MiB stacks + AST walk cap 512).

## `multimedia_camera_framework`

**Path:** `~/multimedia_camera_framework`

### Performance

| Step | Time |
|------|-----:|
| Index | 8.8s |
| Analyze | 0.3s |
| Export | 1.5s |
| **Wall** | **10.9s** |

| Metric | Value |
|--------|------:|
| Files | 1,593 |
| Functions | 25,891 (18,973 defined / 6,918 external) |
| Call edges | 73,136 |
| Direct / indirect / external | 19,427 / **109** / 53,600 |
| Arg-flow edges | 17,245 |
| Parse warnings | 64 |
| Preprocess diagnostics | 4,703 |

Completes. The **109** indirect edges are almost all fuzzer `FuzzedDataProvider` calls; production dispatch is recovered as **direct** CHA. The rise in direct / arg-flow edges vs the previous snapshot is the same C++ name-lookup improvement — namespace-qualified header prototypes; ADL / `using` resolution for unqualified calls — not a resolution loss — every case target above is unchanged. The **external** total additionally reflects the expansion-site attribution fix shipped with #13 (+8,416 external edges): macro-generated call sites are keyed by their invocation instead of their `#define`, so sites that previously collided are no longer merged away. Function counts, the **109** indirect edges and the diagnostics are unaffected by it; the raw-string lexer fix (#14) later took the parse warnings **93 → 91** without moving anything else here. The gMock fallbacks (#15) then took them **91 → 73** and added the 114 recovered mock member prototypes to the external functions; the ten direct edges they moved to **external** are `ON_CALL` arguments that now resolve to the mock's own declared member instead of an unrelated global of the same name. The `...` punctuator fix (#28) then took the parse warnings **73 → 64**; the direct/external swing it caused is `camera_napi_param_parser.h` finally parsing, so its members carry qualified names instead of the unqualified free functions the ERROR node left behind (see the `#28` block at the top).

**Indirect edge changes vs. master** (exact-match metric, both sides):
- **Lost 16** phantom variable targets (`depthProfile`, `depthProfileRet1..4` in `CreateDepthDataOutput`, `infoDumper` in `DumpCameraSummary`): these were bare-stub false positives that the qualified-prototype merge now resolves correctly.
- **Gained 7** lambda callbacks resolved through `CameraFwkMetadataUtils::ForEach` (the qualified prototypes enabled solver tracing into previously unreachable lambda entries).
- Net: 118 → 109 (118 − 16 + 7) is a genuine improvement from prototype-based resolution, not a regression.

### Cases

### 1. `OHOS::CameraStandard::DeferredProcessing::Command::Do` — `Executing`

| Field | Value |
|-------|-------|
| File | `services/deferred_processing_service/src/base/command_server/command.cpp` |
| Line | 33–42 |
| Function | `OHOS::CameraStandard::DeferredProcessing::Command::Do` |
| Dispatch site | `Executing()` (line 37) |
| Resolved targets | **30** (29 defined overrides + pure-virtual `Command::Executing` external) |

`Executing` is pure virtual on `Command`. Source has 29 out-of-line overrides; all 29 resolve. `ServiceDiedCommand` has no `Executing` body (abstract) and is correctly absent.

**Resolved targets:**

- `OHOS::CameraStandard::DeferredProcessing::AddPhotoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::AddPhotoSessionCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::AddVideoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::AddVideoSessionCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::CancelProcessPhotoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::CancelProcessVideoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::Command::Executing`
- `OHOS::CameraStandard::DeferredProcessing::DeletePhotoSessionCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::DeleteVideoSessionCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::EventStatusChangeCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::NotifyJobChangedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::NotifyVideoJobChangedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::PhotoDiedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::PhotoProcessFailedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::PhotoProcessSuccessCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::PhotoProcessTimeOutCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::PhotoSyncCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::ProcessCachePhotoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::ProcessPhotoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::ProcessVideoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::RemovePhotoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::RemoveVideoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::RestorePhotoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::RestoreVideoCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::VideoDiedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::VideoProcessFailedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::VideoProcessSuccessCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::VideoProcessTimeOutCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::VideoStateChangedCommand::Executing`
- `OHOS::CameraStandard::DeferredProcessing::VideoSyncCommand::Executing`

### 2. `OHOS::CameraStandard::DeferredProcessing::Command::Do` — `GetCommandName`

| Field | Value |
|-------|-------|
| File | `services/deferred_processing_service/src/base/command_server/command.cpp` |
| Line | 33–42 |
| Function | `OHOS::CameraStandard::DeferredProcessing::Command::Do` |
| Dispatch site | `GetCommandName()` (line 35) |
| Resolved targets | **31** |

`DECLARE_COMMAND` inline overrides plus `ServiceDiedCommand` (name only) and the pure-virtual `Command::GetCommandName` external. Matches every command class that exists in tree.

**Resolved targets:**

- `OHOS::CameraStandard::DeferredProcessing::AddPhotoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::AddPhotoSessionCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::AddVideoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::AddVideoSessionCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::CancelProcessPhotoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::CancelProcessVideoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::Command::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::DeletePhotoSessionCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::DeleteVideoSessionCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::EventStatusChangeCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::NotifyJobChangedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::NotifyVideoJobChangedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::PhotoDiedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::PhotoProcessFailedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::PhotoProcessSuccessCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::PhotoProcessTimeOutCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::PhotoSyncCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::ProcessCachePhotoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::ProcessPhotoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::ProcessVideoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::RemovePhotoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::RemoveVideoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::RestorePhotoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::RestoreVideoCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::ServiceDiedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::VideoDiedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::VideoProcessFailedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::VideoProcessSuccessCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::VideoProcessTimeOutCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::VideoStateChangedCommand::GetCommandName`
- `OHOS::CameraStandard::DeferredProcessing::VideoSyncCommand::GetCommandName`

### 3. `OHOS::CameraStandard::CFilter::PrepareDone` — `DoPrepare`

| Field | Value |
|-------|-------|
| File | `mediastream/src/filter/cfilter.cpp` |
| Line | ~101 |
| Function | `OHOS::CameraStandard::CFilter::PrepareDone` |
| Dispatch site | `DoPrepare()` |
| Resolved targets | **18** |

All 17 `CFilter` subclasses that define `DoPrepare` plus the base. Complete vs `::DoPrepare(` in `mediastream/src/filter/`.

**Resolved targets:**

- `OHOS::CameraStandard::AudioCacheFilter::DoPrepare`
- `OHOS::CameraStandard::AudioCaptureFilter::DoPrepare`
- `OHOS::CameraStandard::AudioEncoderFilter::DoPrepare`
- `OHOS::CameraStandard::AudioForkFilter::DoPrepare`
- `OHOS::CameraStandard::AudioProcessFilter::DoPrepare`
- `OHOS::CameraStandard::CFilter::DoPrepare`
- `OHOS::CameraStandard::CinematicVideoCacheFilter::DoPrepare`
- `OHOS::CameraStandard::DemuxerFilter::DoPrepare`
- `OHOS::CameraStandard::ImageEffectFilter::DoPrepare`
- `OHOS::CameraStandard::MetaCacheFilter::DoPrepare`
- `OHOS::CameraStandard::MetaDataFilter::DoPrepare`
- `OHOS::CameraStandard::MovingPhotoAudioEncoderFilter::DoPrepare`
- `OHOS::CameraStandard::MovingPhotoMuxerFilter::DoPrepare`
- `OHOS::CameraStandard::MovingPhotoVideoEncoderFilter::DoPrepare`
- `OHOS::CameraStandard::MuxerFilter::DoPrepare`
- `OHOS::CameraStandard::SinkFilter::DoPrepare`
- `OHOS::CameraStandard::VideoCacheFilter::DoPrepare`
- `OHOS::CameraStandard::VideoEncoderFilter::DoPrepare`

### 4. `OHOS::CameraStandard::Pipeline::LinkFilters` — `LinkNext`

| Field | Value |
|-------|-------|
| File | `mediastream/src/pipeline/pipeline.cpp` |
| Line | ~243 |
| Function | `OHOS::CameraStandard::Pipeline::LinkFilters` |
| Dispatch site | `LinkNext` |
| Resolved targets | **23** |

The 17 filter subclasses plus `CFilter::LinkNext`, plus the five mock overrides the gMock fallbacks (#15) made indexable — `FilterMock`, `MockFilter`, `MockNextFilter`, `MockPrevFilter`, `TestFilter`. Complete vs `::LinkNext(` in `mediastream/src/filter/` and the mocks in the test tree.

**Resolved targets:**

- `OHOS::CameraStandard::AudioCacheFilter::LinkNext`
- `OHOS::CameraStandard::AudioCaptureFilter::LinkNext`
- `OHOS::CameraStandard::AudioEncoderFilter::LinkNext`
- `OHOS::CameraStandard::AudioForkFilter::LinkNext`
- `OHOS::CameraStandard::AudioProcessFilter::LinkNext`
- `OHOS::CameraStandard::CFilter::LinkNext`
- `OHOS::CameraStandard::CinematicVideoCacheFilter::LinkNext`
- `OHOS::CameraStandard::DemuxerFilter::LinkNext`
- `OHOS::CameraStandard::FilterMock::LinkNext`
- `OHOS::CameraStandard::ImageEffectFilter::LinkNext`
- `OHOS::CameraStandard::MetaCacheFilter::LinkNext`
- `OHOS::CameraStandard::MetaDataFilter::LinkNext`
- `OHOS::CameraStandard::MockFilter::LinkNext`
- `OHOS::CameraStandard::MockNextFilter::LinkNext`
- `OHOS::CameraStandard::MockPrevFilter::LinkNext`
- `OHOS::CameraStandard::MovingPhotoAudioEncoderFilter::LinkNext`
- `OHOS::CameraStandard::MovingPhotoMuxerFilter::LinkNext`
- `OHOS::CameraStandard::MovingPhotoVideoEncoderFilter::LinkNext`
- `OHOS::CameraStandard::MuxerFilter::LinkNext`
- `OHOS::CameraStandard::SinkFilter::LinkNext`
- `OHOS::CameraStandard::TestFilter::LinkNext`
- `OHOS::CameraStandard::VideoCacheFilter::LinkNext`
- `OHOS::CameraStandard::VideoEncoderFilter::LinkNext`

### 5. `OHOS::CameraStandard::CaptureSession::AddOutput` — `CanAddOutput`

| Field | Value |
|-------|-------|
| File | `frameworks/native/camera/base/src/session/capture_session.cpp` |
| Line | ~1272 |
| Function | `OHOS::CameraStandard::CaptureSession::AddOutput` |
| Dispatch site | `CanAddOutput` |
| Resolved `CanAddOutput` targets | **18** |
| Distinct callee names on the line (what `eval_check` asserts) | **19** |

The two differ, and only the first is a dispatch result. The dispatch resolves **18**: base `CaptureSession::CanAddOutput` plus every session subclass that overrides it in `frameworks/native/camera/`. `CaptureSessionForSys` has no override (stitching calls it through inheritance). Complete vs `::CanAddOutput(` definitions, and it is the list below. The 19th name is `__builtin_expect`, a *second* call site on the same source line — `CHECK_RETURN_RET_ELOG(isVerifyOutput && !CanAddOutput(output), …)` expands to both — and `eval_check.py` counts distinct callee names per (caller, line), not per call site. It is a coarseness of the check, not a target of this dispatch.

**Resolved targets:**

- `OHOS::CameraStandard::ApertureVideoSession::CanAddOutput`
- `OHOS::CameraStandard::CaptureSession::CanAddOutput`
- `OHOS::CameraStandard::FluorescencePhotoSession::CanAddOutput`
- `OHOS::CameraStandard::HighResPhotoSession::CanAddOutput`
- `OHOS::CameraStandard::MacroPhotoSession::CanAddOutput`
- `OHOS::CameraStandard::MacroVideoSession::CanAddOutput`
- `OHOS::CameraStandard::NightSession::CanAddOutput`
- `OHOS::CameraStandard::PanoramaSession::CanAddOutput`
- `OHOS::CameraStandard::PhotoSession::CanAddOutput`
- `OHOS::CameraStandard::PhotoSessionForSys::CanAddOutput`
- `OHOS::CameraStandard::PortraitSession::CanAddOutput`
- `OHOS::CameraStandard::ProfessionSession::CanAddOutput`
- `OHOS::CameraStandard::QuickShotPhotoSession::CanAddOutput`
- `OHOS::CameraStandard::ScanSession::CanAddOutput`
- `OHOS::CameraStandard::SlowMotionSession::CanAddOutput`
- `OHOS::CameraStandard::StitchingPhotoSession::CanAddOutput`
- `OHOS::CameraStandard::VideoSession::CanAddOutput`
- `OHOS::CameraStandard::VideoSessionForSys::CanAddOutput`

## clang/test (llvm-project)

`--jobs 8`, `--timeout-secs 180`. Check: no hang, no stack overflow.

| Subtree | TUs | Index | Analyze | Export | Result |
|---------|----:|------:|--------:|-------:|--------|
| `Preprocessor` | 371 | 1.0s | 0.0s | 0.1s | completes |
| `Lexer` | 138 | 0.2s | 0.0s | 0.0s | completes |
| `Parser` | 325 | 1.4s | 0.0s | 0.2s | completes |
| `CXX` | 918 | 0.5s | 0.0s | 0.1s | completes |
| `Sema` | 1,379 | 3.7s | 0.1s | 0.4s | completes (includes `deep_recursion.c`) |

---

# Appendix — Re-runnable regression checks

The corpora are pinned to fixed upstream revisions in `scripts/eval_expected.json`
(`repo` + `rev` + checkout `dir`), so the counts below can be re-captured by anyone:

| Corpus | Repository | Revision |
|--------|------------|----------|
| `drivers_hdf_core` | `github.com/openharmony/drivers_hdf_core` | `cdc75a20bb8f` |
| `hiviewdfx_hiview` | `github.com/openharmony/hiviewdfx_hiview` | `92408e2072bd` |
| `multimedia_camera_framework` | `github.com/openharmony/multimedia_camera_framework` | `8ffd69dcd47f` |

`scripts/fetch_corpora.py` shallow-fetches each corpus at its pinned revision into the
corpus base (`~` by default, or `--base` / `$TRACE_CORPUS_BASE`); with `--update` it moves
a clean existing checkout to the pin; it never touches a non-empty directory that is not a
git checkout. `scripts/eval_check.py` first verifies every checkout is at the pinned revision
**and clean** (`git status --porcelain` empty — analysis discovers files from the worktree, so
edits or untracked sources move the counts just like another revision); either problem fails
that corpus unless `--skip-rev-check` / `--allow-dirty` downgrade it to a warning. It then
re-analyzes the three corpora fresh and asserts:

1. **Global metrics** — files, functions (defined/external), call edges by resolution
   (direct/indirect/external), arg-flow, diagnostics, `dlsym` PAG edges. Diagnostics,
   `dlsym`, and **indirect** edges must match **exactly** (they are correctness
   invariants); bulk function/edge/arg-flow totals use tolerance bands because the
   parallel index drifts a little run-to-run.
2. **Dispatch-site checks (exact, name-based)** — the 12 HDF hubs
   (`DeviceNodeExtDispatch` 74 … `WorkEntry` 20, linux `osal_workqueue.c`), the 7
   hiview CHA/fn-ptr sites (`Plugin::OnEventProxy`→23 … `GetHandlerInfo`→2 at line 62),
   and the 5 camera cases (`Command::Do` 31/30, `CFilter::PrepareDone` 18,
   `Pipeline::LinkFilters` 23, `CaptureSession::AddOutput` 19). These are the
   eval-report correctness numbers, guarded against silent drift.
3. **C++-slice production probes** — defined overload groups split by scalar type
   (hiview ≥120, camera ≥240), template member call sites that carry resolution
   records (camera ≥15 distinct `…<…>` callee texts), and a **calibration probe**:
   external-class template sites (`MetaHdr` `Set<Tag>`/`Get<Tag>`) must stay
   unresolved (~1,243) instead of degrading into noise edges. Plus an exact
   **phantom-symbol probe** (camera, must be 0): no function named `func` in
   `dps.h`, the template parameter the split `-> *` used to intern as a
   symbol (#37). Bulk totals carry ±120/±420 bands and so cannot pin a
   single fabricated symbol; probes like this one are where that belongs.

```bash
python3 scripts/fetch_corpora.py              # once; --base DIR to keep the checkouts elsewhere
python3 scripts/eval_check.py                 # all three corpora, 800k pops, --jobs 8
python3 scripts/eval_check.py hdf camera      # subset (--corpus-base DIR if not under ~)
```

## Attributing a change: baseline vs. branch

`eval_check.py` says whether the numbers still hold, not which change moved
them. Every "vs. master" delta in this report — including the per-name counts
in the `#28` section — comes from running the corpora **twice**, once with a
binary built from `master` and once with the branch, and diffing the two
databases. Do not attribute a delta without the baseline side: the committed
report can itself be stale relative to `master`'s own binary, which has
produced false attributions before (the `u"…"` column shift blamed on #15 was
really #14's literal lexing).

```bash
set -euo pipefail

# Pin the baseline to a REVISION, not the moving `master` branch: once master
# advances, a recipe that says `master` stops reproducing these numbers. This
# is the commit the #28 section was captured against.
BASELINE=${BASELINE:-2af1eb1}
export TRACE_CORPUS_BASE=/private/tmp/corpora
WORK=$(mktemp -d)
WT="$WORK/baseline-wt"
trap 'git worktree remove --force "$WT" 2>/dev/null || true' EXIT

# Baseline binary from a detached worktree (~35s to build).
git worktree add --detach "$WT" "$BASELINE"
( cd "$WT"
  cargo build --release -p trace-cli
  cargo build --release -p trace-cli --examples )
cp "$WT/target/release/trace" "$WORK/trace_base"

# Branch binary. Build the bin AND the examples: `--examples` alone leaves
# target/release/trace stale, which silently compares the baseline to itself.
cargo build --release -p trace-cli
cargo build --release -p trace-cli --examples
cp target/release/trace "$WORK/trace_new"
if cmp -s "$WORK/trace_base" "$WORK/trace_new"; then
    echo "baseline and branch binaries are identical -- one side did not rebuild" >&2
    exit 1
fi

# One corpus set per binary (~4 min each). eval_check's exit codes:
#   0  everything matched
#   1  some expectation missed
#   2  the run is not usable at all -- corpus missing / at the wrong
#      revision / dirty, binary not found, or `trace analyze` failed
# Exit 2 is fatal on BOTH sides. Exit 1 is expected on the baseline (the
# committed expectations describe the branch, so the baseline necessarily
# misses them -- that is the delta being measured) but is a regression on
# the branch side, so it is only tolerated for `base`.
for side in base new; do
  mkdir -p "$WORK/$side"
  rc=0
  python3 scripts/eval_check.py --bin "$WORK/trace_$side" \
      --corpus-base "$TRACE_CORPUS_BASE" --outdir "$WORK/$side" \
      > "$WORK/$side.log" 2>&1 || rc=$?
  if [ "$rc" -ge 2 ] || { [ "$side" = new ] && [ "$rc" -ne 0 ]; }; then
      echo "eval_check failed on the $side side (exit $rc):" >&2
      cat "$WORK/$side.log" >&2
      exit "$rc"
  fi
  tail -1 "$WORK/$side.log"
done

# Do not diff on the strength of the exit codes alone. eval_check turns its
# own crashes into exit 2, but a run killed from outside (OOM, SIGKILL)
# cannot report anything, and would leave a truncated log plus databases
# from the corpora it did finish. So require each side to have reached its
# summary line, and both sides to have run the same number of checks --
# a short count means one side stopped early.
for side in base new; do
  tail -1 "$WORK/$side.log" | grep -qE '(PASS|FAIL): [0-9]+ checks' ||
    { echo "$side run did not reach its summary -- log truncated" >&2; exit 1; }
  for c in hdf hiview camera; do
    [ -s "$WORK/$side/eval_check_$c.db" ] ||
      { echo "missing database: $WORK/$side/eval_check_$c.db" >&2; exit 1; }
  done
done
checks() { sed -nE 's/.*: ([0-9]+) checks.*/\1/p' "$1" | tail -1; }
[ "$(checks "$WORK/base.log")" = "$(checks "$WORK/new.log")" ] ||
  { echo "sides ran different check counts -- one stopped early" >&2; exit 1; }

# Metric diff: check lines only, so the outdir paths in the progress lines
# do not show up as differences. A non-zero diff is the point here, so it
# must not end the script.
metrics() { grep -E '^(  ok|FAIL)' "$1" | sed 's/(expected.*//'; }
diff <(metrics "$WORK/base.log") <(metrics "$WORK/new.log") || true

# Then run any of the SQL in the sections above against the two DBs:
echo "databases: $WORK/{base,new}/eval_check_{hdf,hiview,camera}.db"
```

Compare the **exact** metrics (`diagnostics`, `edges_indirect`, `dlsym_edges`,
dispatch target sets) rather than bulk totals — the parallel index drifts run
to run, so a small function/edge difference between two runs of the *same*
binary is noise, not a finding. The probes are `min`/`band` thresholds, so they
confirm nothing collapsed; they do not pin a number to diff against.

Exit codes: **0** all checks pass (**90 checks**, three of them the revision pins; `master` has
missed 14 since #66, see the latest comparison above), **1** some expectation was missed, **2** the run is not
usable at all and its numbers must not be read — a corpus missing, at the wrong revision
or dirty (unless `--skip-rev-check` / `--allow-dirty` downgrade it), or `trace analyze`
itself failing. The 1-vs-2 split is what lets the baseline comparison above tolerate a
baseline that misses the current expectations without also swallowing a broken run. The expectation values were last re-captured on 2026-09-04 on top of
2af1eb1 for the `...` punctuator fix (#28), at the pinned revisions; earlier captures were
2026-09-04 on 168e643 (#15, camera only) and 2026-09-02 after `Improve cpp name lookup`.
The metric tables in the corpus sections above show those same values.
