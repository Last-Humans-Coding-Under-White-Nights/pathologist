# Memory ownership investigation

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

Preprocessing's serial and worker filesystem memos are released before header
IR construction. On Linux/glibc, `malloc_trim(0)` returns unused pages after
the preprocessing and header phases and after indexing caches and the local
worker pool drop; it never runs while workers allocate, since a trim locks
every arena and takes hundreds of milliseconds on a large heap. Other
platforms retain descriptor sharing and cache-lifetime improvements, with no
allocator-specific reclamation. Parse workers take units in order and run at
most four units per worker ahead of the ordered merge, which bounds pending IR
the way small batches did without idling workers behind a slow unit.

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
