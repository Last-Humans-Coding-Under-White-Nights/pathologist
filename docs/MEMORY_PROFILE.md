# Memory ownership investigation

The latest measurements are in [Preprocessing cache budget and configured
streaming](EVAL_REPORT.md#preprocessing-cache-budget-and-configured-streaming--2026-10-05).
The preceding [ownership research](EVAL_REPORT.md#preprocessing-and-indexing-memory-research--2026-10-05)
identified unused resident LineMap capacity, duplicated diagnostics, aggregate
source-cache retention and whole-corpus configured-unit retention. The retained
implementation covers the first three items and ordinary configured streaming
from item 4. Explicit-image streaming and expansion-cache compaction/spilling
remain unimplemented; remaining floors are listed with those measurements.

This document records the 2026-09-15 camera-corpus investigation and its
follow-ups. The later `ability_ability_runtime` measurements, including compact
cached header type tables and earlier release of discarded payloads, are in
[EVAL_REPORT.md](EVAL_REPORT.md#compacting-cached-header-type-tables--2026-09-28);
the macOS phase peaks, live-byte floors and CPU profile of the same corpus,
with the decision on an incremental IR cache (#175), are in
[PERFORMANCE_REVIEW.md](PERFORMANCE_REVIEW.md#incremental-per-tu-ir-cache-macos-measurements-and-decision-175).

Measured 2026-09-15 on the pinned OpenHarmony camera corpus (744 TUs, 849
headers), with 16 indexing workers and the existing spilling/small-batch
changes. The aim was to distinguish retained data from allocator retention.
The measurements below describe the baseline before the follow-up changes
reported at the end of this document.

## What occupies memory

| Retained component | Measured footprint | Interpretation |
|---|---:|---|
| Cached header `UnitIndex` values | approximately 383 MiB | Mostly repeated imported type information |
| Type tables within those header units | approximately 357 MiB | **93% of the header IR footprint**; included in the preceding row |
| Filesystem probe/path caches | at least 168 MiB at heap peak | Full candidate paths cached per thread, including unsuccessful probes |
| Cached macro operation payloads | approximately 114 MiB | Owned macro definitions and token vectors copied into expansion logs |
| Cached diagnostics | approximately 24 MiB | Repeated owned paths/messages across expansion records |
| Cached expansion text + LineMaps | approximately 26 MiB | About 6 MiB text and 20 MiB mappings |
| Final merged `Program` | approximately 94 MiB | Much smaller than intermediate header IR |
| Merge dedup state within `Program` | approximately 22 MiB | Included in the preceding row; largely unnecessary once merging/finalization ends |

These are **different measurements of overlapping stages**, not rows to add
into a single peak total. Header/type/macro/diagnostic/Program footprints came
from temporary bulk-clone probes, measuring changes in glibc allocated bytes
around the clones. They include owned nested allocations but exclude shared
`Arc` payloads and can omit spare vector capacity; they are useful estimates,
not an exact recursive ownership census. The temporary clones were not used
for the baseline RSS or Massif measurements and were removed afterward.

The header cache contained **246,345 type records**, while the merged program
contained **7,500**. Cached units repeatedly import nested headers' types into
their own tables. `TypeTable` stores descriptors in both `types` and `intern`,
and recursive `TypeDesc` values own boxed pointees, parameter vectors,
aggregate fields, and strings. Clone/import operations copy that structure.
The merged program's type table itself was only approximately **10 MiB**.
The problem is primarily intermediate duplication, rather than final types.

There were **1,951 cached include expansions**. Their text was only 6,270,434
bytes. Macro operations cloned to 119,048,992 allocated bytes; diagnostics to
25,204,336 bytes; maps to 20,443,824 bytes. Spilling more text alone therefore
cannot eliminate most of the expansion-cache footprint.

Before TU parsing, temporary counters recorded **1.11–1.12 million probe-cache
entries across threads**, **117 MiB of path-key bytes**, and approximately
1.3 million entries of hash-table capacity. This is not the number of unique
paths: the same candidates occur in separate thread-local caches. The entire
source tree's C++/header contents were only **15.4 MiB**.

## RSS is substantially larger than retained allocations

Native camera measurements used glibc `mallinfo2` and `/proc/self/status`,
without cloning objects:

| Boundary | Process RSS | Allocator allocated space | Free arena space |
|---|---:|---:|---:|
| Include graph complete | 201 MiB | 175 MiB | 21 MiB |
| Header warming complete | 426 MiB | 398 MiB | 24 MiB |
| Preprocessing complete | 622 MiB | 504 MiB | 135 MiB |
| Header IR complete, before merging | 950 MiB | 913 MiB | 43 MiB |
| Indexing returned, caches/local pool dropped | 1,231 MiB | 302 MiB | 951 MiB |
| Analysis returned | 1,239 MiB | 339 MiB | 924 MiB |

"Allocated space" is `uordblks + hblkhd`, including allocator rounding and
mmapped allocation regions; it is not just useful object bytes. Free arena
space can be resident or already unmapped/decommitted, so these columns must
not be added to reconstruct RSS. Stage snapshots also miss transients between
boundaries. They clearly show that freed parsing allocations remain resident
in glibc arenas after indexing.

In a separate diagnostic run, `malloc_trim(0)` immediately after indexing
reduced RSS from **1,209 to 427 MiB**, with allocated space unchanged at
approximately **298 MiB**. The trim took approximately **0.29 s**. Later
analysis/export RSS stayed around 450–465 MiB with additional boundary trims.
This does **not** eliminate the earlier live-allocation peak, and peak RSS
recorded by the OS does not reset after trimming.

HDF with eight workers showed the same pattern: after indexing, RSS was
**468 MiB**, allocated space **192 MiB**, and free arena space **286 MiB**.
A diagnostic trim reduced RSS to **249 MiB** without changing allocated space.

## Independent heap allocation profile

Valgrind Massif, run before the temporary clone probes, found peak useful heap
of **891.5 MiB** and approximately **111.6 MiB** of its modeled allocator
overhead. It recorded approximately **26 GiB** of cumulative allocation/free
traffic on its byte-based time axis; this is churn, not simultaneously live
memory. Its execution time is not a benchmark of the native program.

The peak allocation tree was partitioned by stack frames, counting each leaf
once:

| Allocation stack category | Useful bytes at peak |
|---|---:|
| Type descriptors/tables | 246.9 MiB |
| Filesystem/path caches | 168.1 MiB |
| Preprocessor | 149.0 MiB |
| Other merge/symbol-table allocations | 64.3 MiB |
| Raw source/include graph | 15.4 MiB |
| Tree-sitter | 13.8 MiB |
| Unresolved below profiler threshold | 234.1 MiB |

The 1% reporting threshold hid many individual allocation paths, so named
categories are lower bounds. In particular, 234 MiB of generic string/vector/
hash-table allocations cannot be assigned confidently from that tree. The
separate object-clone estimates expose much more of the type-table footprint.
Tree-sitter AST storage is a relatively small share of this workload's peak.

## Changes with the greatest potential

1. **Share imported header types.** The largest measured intermediate owner is
   header type tables. Use an immutable declaration/type environment with
   per-unit additions/remaps, or represent recursive types as a graph of
   `TypeId` references instead of embedded descriptor trees. Avoid importing
   the same included closure into every header's own `TypeTable`. This is a
   larger change: preserve TU-local IDs, tag completion, aliases, variants,
   anonymous scopes, and first-definition selection. The measured 357 MiB is
   a target footprint, not a guaranteed saving.
2. **Release include-discovery worker probe caches.** Include-graph construction
   uses Rayon's global pool; later indexing creates a separate pool. The first
   pool's thread-local path memos remain resident without helping the second
   pool. Clear and release those memos at the phase boundary while retaining
   shared directory listings. Longer-term, key probes by interned directory
   plus filename, or use bounded/shared caches. Clearing must release bucket
   capacity as well as keys; the baseline epoch reset called `clear()` only.
3. **Share immutable macro definitions/replacement tokens.** `MacroOp::Define`
   owns a `MacroDef`, whose baseline vectors/strings cloned deeply. Cached operation
   suffixes also include nested headers' operations. Shared immutable token/
   parameter storage, or shared operation records, can remove copies while
   retaining every ordered define/undef event and its exact token origins.
4. **Return freed allocator pages at suitable boundaries.** Trimming after
   indexing/analysis is a measured way to reduce later RSS, especially in a
   long-lived C API host. Test periodic trims at ordered parse-batch boundaries
   to assess peak reduction and latency. This complements reducing live data;
   it cannot replace it. Any implementation needs platform-specific handling.
5. **Discard merge dedup state once merging is finished.** Approximately 22 MiB
   remains in the final program's dedup maps. Clear it in consumers that will
   not perform further merges, after all finalization passes. Do not remove
   the state from callers using incremental merge APIs.
6. **Share cached diagnostic messages/origins.** Approximately 24 MiB is in
   cached diagnostic copies. Immutable shared records can preserve emission
   order and original paths without cloning nested messages repeatedly.

Dropping header bodies after merging has limited upside here: types occupy
93% of the cached header IR. Reducing parsing worker count or spilling more
source text also misses the largest owner. The first two changes above address
hundreds of MiB; macro sharing and allocator retention are additional major
targets.

## Reproduce and inspect

The new Linux/glibc observer requires a C compiler and records stage RSS,
allocated space, free arenas, and JSON/log artifacts:

```bash
python3 scripts/profile_memory.py ~/multimedia_camera_framework --jobs 16 \
  --outdir /tmp/memory-camera
python3 scripts/profile_memory.py ~/multimedia_camera_framework --jobs 16 \
  --trim --outdir /tmp/memory-camera-trim
# Optional detailed allocation stacks; several minutes, with Valgrind installed:
python3 scripts/profile_memory.py ~/multimedia_camera_framework --jobs 16 \
  --massif --outdir /tmp/memory-camera-massif
```

The script builds the current release binary before running. The new tool's
Massif threshold is 0.1% for finer attribution; the investigation's original
profile used 1%. Keep the worker count fixed when comparing memory profiles.
Temporary observer libraries clean up automatically. Results go under the
selected output directory; database/profile artifacts should not be committed.

The baseline observer, trim, Massif, and completed clone-audit camera runs
produced identical rows in all 12 analysis tables when compared with the
preceding implementation, excluding `analysis_run` metadata. Temporary
production probes were restored byte-for-byte and the release binary rebuilt.

## Implemented follow-up: cache lifetimes and shared macro storage

The quick follow-up releases global Rayon discovery workers' thread-local
path/canonicalization caches after include scanning. Epoch resets now discard
hash bucket capacity too. Shared directory listings remain available for
preprocessing. Macro definitions use immutable `Arc<[Token]>` / `Arc<[String]>`
payloads, making table/log clones share their storage; invocation painting
continues to create independent tokens. This changes the public `MacroDef`
field types: hand-built definitions convert vectors with `.into()`.

CLI and C API consumers release merge dedup tables after finalization. The
public program-building APIs keep that state for callers that merge further.

On the same camera corpus with 16 jobs, a normal release run reduced peak RSS
from **1,282,528 KiB to 1,127,636 KiB** (approximately **151 MiB / 12.1%**).
Elapsed time was 14.61 seconds versus the earlier 16.81-second sample; these are
single-run measurements, so the timing difference is not a controlled speed
claim. All 12 SQLite analysis tables match the baseline, excluding run metadata.

A separate native observer run recorded these live-allocation improvements:

| Boundary | Baseline allocated MiB | Follow-up allocated MiB |
|---|---:|---:|
| After include graph | 174.7 | 22.9 |
| After preprocessing | 503.8 | 250.2 |
| After header lowering | 913.1 | 659.6 |
| After indexing and merge state release | 302.1 | 128.5 |

These include scheduling variation, but show why RSS alone understates the
savings: after indexing the allocator still retains approximately 969 MiB of
free arena memory. Production allocator trimming was not added in this change.
Repeated header type tables remain the next major live-memory target.

Reproduction artifacts: `/tmp/trace-camera-fast-memory.log`,
`/tmp/trace-camera-fast-memory.db`, and `/tmp/trace-camera-fast-profile/`.
The workspace suite passed all 802 tests, including cache-capacity release
and shared-definition storage checks.

The pinned HDF, Hiview, and camera corpus evaluation also passed all 93 checks.

## Implemented follow-up: shared type descriptors and page reclamation

`TypeInfo.desc`, type intern keys, and typedef alias values now share immutable
`Arc<TypeDesc>` payloads. Simultaneously live tables use a content-addressed
weak pool, sharded by descriptor hash so parallel workers rarely contend on a
lock. Hash collisions are resolved by full descriptor equality; sharing
never selects a different definition or changes insertion order. Dead weak
entries are pruned per shard, and the pool disappears with the last table.
Table-local type IDs, tag indexes, layouts, and completion flags stay separate.
Variant aggregate union replaces the descriptor without modifying another
table's payload. Tests cover shared storage, isolated aggregate mutation,
released payloads, and deterministic output across indexing worker counts.

For the measurements below, preprocessing's serial and worker filesystem memos
were released before header IR construction. On Linux/glibc, `malloc_trim(0)`
returned unused pages after the preprocessing and header phases and after
indexing caches and the local worker pool dropped; in that implementation it
ran only after workers stopped, since a trim locks every arena and takes
hundreds of milliseconds on a large
heap. Other platforms retain descriptor sharing and cache-lifetime improvements,
with no allocator-specific reclamation. Parse workers then took units in order
and ran at most two units per worker, between 4 and 32 in total, ahead of the ordered
merge. This bounded pending IR, but a slow unit could still fill the window
and park the other workers. Admission now uses
[estimated bytes with a count safety cap](PERFORMANCE_REVIEW.md#6-make-parsing-memory-limits-independent-of-cpu-count);
the current comparison is in
[Byte-budgeted indexing window](EVAL_REPORT.md#byte-budgeted-indexing-window).

Final normal release benchmarks, with compilation excluded:

| Corpus | Workers | Baseline peak KiB | Final peak KiB | Reduction | Baseline elapsed | Final elapsed |
|---|---:|---:|---:|---:|---:|---:|
| Camera | 16 | 1,282,528 | 619,868 | **51.7%** | 16.81 s | 14.10 s |
| HDF | 8 | 517,660 | 241,332 | **53.4%** | 10.73 s | 9.55 s |

The baseline already includes source spilling and smaller parse batches.
Earlier camera repeats during this follow-up reached 632,516–637,048 KiB
(approximately 50.3–50.7% below baseline), so the precise reduction varies
with scheduling and allocator state. These are measurements of these pinned
corpora on Linux/glibc, not a universal memory budget or controlled timing
study. Worker counts were not reduced.

An intermediate native observer run with descriptor sharing and phase trims
recorded approximately **363 MiB allocated after header IR construction**
(913 MiB at baseline) and **89 MiB after indexing** (302 MiB at baseline).
It preceded the final queue/pool bookkeeping refinements. A separate 50 ms RSS
sampler placed the remaining camera peak during TU parsing, after header IR
construction; its sample peak was 634,664 KiB. After analysis, RSS was around
320 MiB instead of the baseline's approximately 1,239 MiB.

All 804 workspace tests passed and all 12 SQLite analysis tables matched
baseline data for both camera and HDF, excluding run metadata. Benchmark logs
are `/tmp/trace-camera-memory-verified.log` and
`/tmp/trace-hdf-memory-final.log`; intermediate stage measurements are in
`/tmp/trace-camera-types-trim-profile/`.

Rust API migration: `TypeInfo.desc` is now `Arc<TypeDesc>`; match with
`info.desc.as_ref()` and use `info.desc.as_ref().clone()` when an owned
mutable descriptor is needed. `TypeTable::all_aliases()` exposes shared alias
values. Serde representation and the C ABI/SQLite schema are unchanged.

## Implemented follow-up: struct compaction, expansion sharing, and lifecycle reclamation

Rules and API migration contracts are documented in [Type storage and TypeFields](ANALYSIS.md#typefields-and-layout-storage), [Constraint representation and accessors](ANALYSIS.md#constraint-representation-and-accessors), and [Post-solve flow-release lifecycle](ANALYSIS.md#post-solve-flow-release-lifecycle). Measured probe details and reproduction steps are documented in [Evaluation report](EVAL_REPORT.md#struct-compaction-expansion-sharing-and-lifecycle-reclamation--2026-10-01).

1. **`TypeInfo` and `TypeLayout` struct compaction (`trace-ir`)**:
   - `TypeInfo` inline size reduced from 104 B to 40 B (−61.5%).
   - `TypeLayout` inline size reduced from 72 B to 8 B (−88.9%).
   - Aggregate field layouts wrapped in `TypeFields(Option<Box<IndexMap<FieldId, FieldLayout>>>)`: types without fields store `None`, taking 0 heap bytes and only 8 inline bytes instead of 72 inline bytes for an empty `IndexMap`.
   - `TypeTable::compact_for_header_merge()` drops `TypeFields` heap allocations to `None` via `layout.fields.clear()` and calls `shrink_to_fit()` on `types` and `aliases` vectors.
2. **`Token` and `LineMapEntry` struct compaction (`trace-preproc`)**:
   - `Token` inline footprint reduced from 96 B to 56 B (−41.7%) by sharing macro expansion tracking in `Option<Arc<TokenMacroProvenance>>` with copy-on-write (`Arc::make_mut`), avoiding allocation churn during repeated argument substitution.
   - `LineMapEntry` size is guarded at 40 B by regression test `line_map_entry_size` (Rust compiler layout automatically packs non-`repr(C)` fields by alignment).
3. **`MacroOp` and `IncludeExpansion` representation (`trace-preproc`)**:
   - `MacroOp` representation changed to `Define(Arc<str>, Arc<MacroDef>)` and `Undef(Arc<str>)`, cutting size from 64 B to 24 B (−62.5%). `MacroTable` stores definitions as `Arc<MacroDef>`, eliminating heap string allocations and deep definition copies during directive logging, cache-frame capture, and replay. When no cache frames are open during translation unit preprocessing, macro op construction is skipped entirely.
   - `IncludeExpansion` collections (`ops`, `diagnostics`, `guards`, `nested_variants`, `inlined`, `covers`) converted from `Arc<Vec<T>>` to direct `Arc<[T]>` slices, eliminating unused vector capacity and indirection across cached include expansions.
4. **Lifecycle reclamation and AST pruning (`trace-parse`, `trace-analysis`)**:
   - Parse trees and transient expression caches dropped immediately upon AST lowering completion (`ctx.tree = None`).
   - Filesystem directory listing cache (`DIR_LISTINGS`) cleared alongside thread-local path caches post-preprocessing.
   - `UnitIndex.held_headers` shares `expansion.inlined` as `Arc<[PathBuf]>` without copying path vectors.
   - Cached header `UnitIndex` instances shrunk to fit prior to caching in `HeaderIr`.
   - `HeaderIr` and `include_expansion_cache` dropped immediately as translation unit parsing completes before program finalization.
   - Program IR flow memory released via `program.release_flow()` post-solving, prior to SQLite export.
   - PAG `Constraint` inline size reduced from 48 B to 24 B (−50.0%) by boxing field access metadata (`FieldAccess.field_name` stored as `Arc<str>`, eliminating heap allocations in solver worklist propagation); eliminated cloning and destructive modification of call edges and wired argument flows during analysis.

### End-to-end benchmark measurements

End-to-end peak memory footprint, RSS, and runtime measurements comparing base (`18303d6`) and head on the pinned evaluation corpora (camera and HDF) are recorded in [Evaluation report](EVAL_REPORT.md#struct-compaction-expansion-sharing-and-lifecycle-reclamation--2026-10-01). All 12 SQLite analysis tables remain byte-identical to base on camera, HDF, and hiview.




## LLVM monorepo startup peak (#209)

Reported: `trace analyze` on a full `llvm/llvm-project` checkout reaches
roughly 10 GB before any translation unit is indexed, then dies of OOM.
Reproduced 2026-10-07 on macOS (8 GB, 8 cores, `--jobs` default 8, no
compilation database, no `--include`) with `llvm-project` at
`cc67eac964112fe043aa3527c2d63d2dd55f56ee`: 53,990 TUs, 18,212 headers,
72,202 project files, 780 MB of C/C++ source text, and an inferred search
list of **2,823 directories** (every directory holding a header, plus every
`include` directory). `phys_footprint` was sampled every 100 ms by a
watchdog that kills the analyzer at 5 GiB.

| Build | Include graph | Peak footprint | Where |
|---|---:|---:|---|
| Baseline (`2e17bd8`) | killed at 119 s, unfinished | > 5.00 GiB | include-graph scan, before `include-graph:` is printed |
| Listing-settled misses not memoized | killed at 192 s, unfinished | > 5.00 GiB | same |
| + absent directories settled from listings in hand | 376 s | 1.06 GiB | include graph complete |
| + one search walk per spelling | 36.5–38.5 s (3 runs) | 1.05 GiB | include graph complete |
| + one lock per walk, existing levels recorded on the way down (retained) | **16.6–17.7 s** (2 runs) | **1.06 GiB** | include graph complete |

### Root cause: memo entries per (search directory × spelling), per thread

`trace_ir::is_file_cached` memoized every candidate path it was asked
about, per thread, keyed by the full path bytes. Include resolution tries a
spelling under each search directory until one holds it, so a spelling such
as `llvm/ADT/StringRef.h` produces a candidate under every one of the 2,823
directories that sort before `llvm/include`, and `<vector>` produces one
under all of them. The directory-listing shortcut (#83) saved the `stat`
for a name the parent's listing does not hold, but still recorded the
answer — and for a candidate whose parent directory does not exist
(`<dir>/llvm/ADT` under almost every `<dir>`) it could not consult a
listing at all, so it both `stat`ed the path and memoized it, while
`DIR_LISTINGS` recorded an entry per absent directory. Each of the 8 scan
workers built its own memo, since the same spellings recur in every file.
On the `llvm/` subtree alone (373 search directories) the memo held 920,098
entries with 170 MB of keys at the end of the scan; the full tree's memo
never finished filling.

The retained change settles both from what exists: a miss the parent's
listing settles is not memoized (the listing answers a repeat as cheaply),
and a candidate under an absent directory is settled from the nearest
listing already in hand -- the listed ancestor that does not hold the child
on the way down, one lock and one lookup per level -- with the absent
directories neither read nor recorded
([PREPROCESSOR.md](PREPROCESSOR.md#include-resolution)). Reading starts only
below a directory a probe asked for directly: the existing levels between
it and the candidate are read and recorded on the way down (bounded by the
directories that exist), while ancestors are never read on demand -- doing
so snapshotted directories above the analyzed tree, and a sibling tree
created later in the same epoch (one test's scratch directory after
another's) became invisible when a first version did that. Recording the
absent child at the listed ancestor was also tried and dropped: one entry
per (search directory × first spelled component) cost 200 MiB. The memo and
the listings therefore grow with the files and directories that exist, and
the full tree's include graph now completes in 1.06 GiB, of which 780 MB
is the source text `IncludeGraph::source_cache` keeps for preprocessing. The static include graph's scan also shares one search
walk per (spelling, includer partition) across every file that spells it,
as the preprocessor's own `include_search_cache` already did; without
that, the probes the memo used to absorb cost 376 s.

The `llvm/` subtree (4,663 TUs, 3,529 headers, 373 search directories)
shows the same change at a scale the baseline completes: include graph
1.5 s and 0.32 GiB before (920,098 memo entries, 170 MB of keys), 1.7 s
and 0.21 GiB after (single runs; a repeated miss now walks the shared
listings instead of hitting a thread-local entry).

### What remains before indexing on the full tree

With the include graph bounded, the run proceeds into the sequential warm
pass: 12,480 reachable headers, 1.13 GiB at its start, 2.03 GiB at header
4,950 after 300 s (about 0.18 MiB per warmed header, retained in the
include-expansion cache and the source cache), where the run was stopped.
The pinned corpora show the change is neutral there: camera 8.1 s → 7.6 s,
HDF 4.7 s → 4.2 s, hiview 1.8 s → 1.6 s wall (single warm-cache runs on the
same machine, `--jobs 8`), peak footprint unchanged (0.24, 0.22, 0.05 GiB),
and every exported table except `analysis_run` identical to the baseline on
all three. The 780 MB of
retained source text and the warm pass's growth on a tree this size are
separate items with their own owners; neither was the reported peak.

### With a compilation database: bounded, but hours of work

A reviewer applying the change with a `compile_commands.json` (6,672
commands) still saw memory climb to about 5 GB and no output for ten
minutes after the `compile_commands:` line. Reproduced with a cmake
configure of `llvm` + `clang` (X86 only, tests off: 3,603 commands for
3,602 sources, 54,376 discovered TUs in all) on the same machine,
`--jobs 8`, 480 s cap:

| Checkpoint | Units indexed | Footprint |
|---|---:|---:|
| `compile_commands:` printed (17.5 s) | 0 | 1.07 GiB |
| 120 s | 371 | 2.19 GiB (peak so far 2.73) |
| 240 s | 624 | 2.96 GiB (peak so far 3.47) |
| 480 s | 1,340 | 3.13 GiB (peak 3.58) |

This is the configured path (`configured.rs`), which the earlier
measurements did not take: it had no progress output at all between the
`compile_commands:` line and the end of indexing, so the run looked
stalled. It now prints `parse: i/N path` every 50 units, like the ordinary
path (`index_item_progress`; `TRACE_INDEX_VERBOSE=1` for every unit).

In this run the footprint oscillates between about 2.1 and 3.5 GiB
rather than growing. The measured configuration had no link-target
metadata (no `link_commands.json`, no link entries in the database, no
CMake File API reply), so `configured::build` merges each unit into the
program in source order as indexed results become available. In this measured
run, indexed but unmerged IR was bounded by the former count window (two units
per worker, 4–32 in all), on top of the 1.07 GiB of
retained source text and include graph. What makes the bound high is
the unit, not the count: on this path every unit lowers its whole
inlined include closure (no shared expansion cache, no PCH header IR;
see [Bound compilation-database IR
accumulation](PERFORMANCE_REVIEW.md#2-bound-compilation-database-ir-accumulation)),
and an LLVM unit's closure is 246 files on average for a command unit
and 627 for a source without a command (the shared fallback
configuration, mostly tests and unittests) -- 18,401 functions per unit
at the median, 29,828 at p90, 40,696 at most. At 2.8 units/s the 54,376
units would take about five hours; 326 fallback units averaged 3.4 s
each and 1,014 command units 2.5 s. Configured admission retains this source-family
count limit; see the
[current policy and validation](PERFORMANCE_REVIEW.md#6-make-parsing-memory-limits-independent-of-cpu-count).
These figures describe that measured revision.

The window bound does not hold when the build metadata names explicit
link targets (`scoped` in `configured.rs`: targets present and not an
unscoped inference). Image selection and weak-symbol resolution need
every unit of a target before any of them can merge, so that branch
keeps each completed `UnitIndex` in `linked_units` until all sources are
indexed, and the inlined include closures accumulate once per TU
regardless of `--jobs`. With a `link_commands.json` or a File API reply
for a tree this size, expect the footprint to grow with the TU count,
not to plateau; that is the retention [PERFORMANCE_REVIEW.md
§2](PERFORMANCE_REVIEW.md#2-bound-compilation-database-ir-accumulation)
describes, and it was not measured here.

Supported ways to analyze this scope today: fewer `--jobs` lowers the
window and so the peak roughly in proportion (`--jobs 4` keeps eight
units in flight instead of sixteen) on the unscoped branch, and does
not bound the scoped one; or analyze a subtree by pointing the analysis
root at it. The compilation database does not limit scope: every TU
discovered under the root is indexed, and a source the database has no
command for takes the shared fallback configuration (`configs_for`),
whose closure is the larger of the two above. A database restricted to
the sources of interest therefore moves the rest to the more expensive
configuration instead of skipping them. Sharing header lowering across
commands of one configuration family is the structural fix and is a
separate change.
