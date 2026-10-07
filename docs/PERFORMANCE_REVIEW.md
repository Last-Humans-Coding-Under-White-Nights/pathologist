# Performance and memory review

This is a historical review of the 2026-09-15 implementation. The current
parallel discovery protocol is described in [PREPROCESSOR.md](PREPROCESSOR.md#parallel-discovery-88),
and later large-corpus memory measurements are in [EVAL_REPORT.md](EVAL_REPORT.md#compacting-cached-header-type-tables--2026-09-28).

For measured ownership, allocator retention, and the largest remaining memory
targets, see [Memory ownership investigation](MEMORY_PROFILE.md).

Reviewed 2026-09-15 at commit `c1c76b6`, using Rust 1.95.0. The initial review below describes that baseline. The implementation and measurements from the follow-up are recorded at the end.

## Measurements

The local OpenHarmony camera corpus has 744 translation units and 849 headers. Release runs used the current binary through `cargo run -p trace-cli --release -- analyze`, default minimal export, no exploration, and no retained debug points-to sets. The release build was completed before the measurements below. `/usr/bin/time -v` measured whole-command elapsed time and peak resident memory, including the small Cargo startup cost.

| Camera corpus | Index | Analyze | Export | Whole command | Peak RSS |
|---|---:|---:|---:|---:|---:|
| 16 workers | 12.9 s | 0.5 s | 1.5 s | 15.49 s | 1,472,376 KiB (1.40 GiB) |
| 4 workers, isolated repeat | 17.0 s | 0.5 s | 1.4 s | 18.13 s | 1,258,396 KiB (1.20 GiB) |

Four workers saved about 209 MiB / 14.5% peak RSS at a 17% whole-command time cost. This is a useful existing option for memory-constrained runs, not a universal faster default. An initial four-worker run, partly overlapping a separate SQLite experiment, gave similar results (18.39 s and 1,253,900 KiB); the table uses the isolated repeat instead.

The 16-worker preprocessing discovery pass took 3.6 s; settling 285 of 744 units took 0.3 s. Header IR construction took 0.7 s for 1,322 expansions of 814 headers. Indexing is approximately 83% of elapsed time; solver optimization alone has little upside for this corpus. The smaller thermal corpus indexed in 0.6 s and exported in 0.1 s, so it is too small for useful memory or scalability conclusions here.

Camera output contained 23,905 functions, 97,422 call edges (168 indirect), and 31,113 argument-flow edges. These are measurements of the locally available source trees, not a claim that the pinned evaluation expectations were verified.

SHA-256 hashes of deterministically ordered rows matched for every analysis table across the first 16-worker run and both four-worker runs, excluding `analysis_run`. A second 16-worker run reported 14.57 s whole-command elapsed time and 1,478,660 KiB peak RSS; it briefly overlapped the table-comparison script, so it is a supporting observation rather than an isolated timing trial.

Measurements are a small sample on one machine with ordinary OS caching. There was no allocation profiler or per-function CPU profile. The code findings below establish avoidable work; their production speedups remain to be measured.

## Prioritized opportunities

### 1. Copy only the requested LineMap interval

**Where:** `crates/trace-preproc/src/preprocessor.rs`, `append_live_chunk`; `crates/trace-preproc/src/line_map.rs`, `slice_from`.

`append_live_chunk(from, to)` calls `src_map.slice_from(from)`, which copies and remaps every entry from `from` to the end of the map. The caller then stops reading when the copied offset reaches `to - from`. Entries after `to` were allocated and remapped unnecessarily. `compose_cache_text` invokes this helper for multiple chunks, so processing many short chunks can repeatedly copy overlapping suffixes. With many chunks across a large map this can approach quadratic work in the number of mappings.

Add a bounded interval operation that finds both endpoints with binary search and copies/remaps only entries in `[from, to)`. Alternatively append the bounded range directly into the destination map. This reduces both CPU work and temporary allocation without changing analysis semantics. Preserve equal-offset entries, original file attribution, and the existing boundary behavior; do not introduce a preceding entry unless its need is separately established.

This is the first preprocessing change worth benchmarking, especially because discovery remains serial by design.

### 2. Bound compilation-database IR accumulation

**Where:** `crates/trace-parse/src/configured.rs`, `build`.

The compilation-database path collects `Vec<ConfiguredUnits>` for all sources, then collects all their `UnitIndex` values into another vector before merging the entire configuration family. Moving those vectors does not duplicate their contents, but every lowered TU and configuration stays alive until the family merge. Orphan headers also use an all-header collection in the parallel branch.

Unlike the ordinary indexing path, configured preprocessing deliberately inlines headers and disables shared expansion caching. Widely included headers therefore contribute repeated IR across many simultaneously retained units. This is a strong structural memory risk for large compilation databases, although the measured camera run used the ordinary path.

Use bounded, ordered production and an incremental family merger. The family merger must retain the same `VariantDedup` state across batches: independently calling `merge_unit_variants` per batch would change how configurations extend shared header definitions and union layouts. Preserve first-encountered definitions, source-scope distinctions, include discovery, and the per-source exploration tally. Orphan headers can use the ordinary batching approach more directly.

### 3. Create secondary SQLite indexes after loading rows

**Where:** `crates/trace-db/src/export.rs`, `export_to_sqlite`; `crates/trace-db/src/schema.rs`.

The exporter creates the entire schema, including eight secondary indexes, before inserting rows. Each insertion therefore maintains those indexes. Split table creation from secondary-index creation, load the rows, then build the same indexes before committing and publishing the database. Keep primary keys and any integrity requirements in place during loading.

A separate Python/SQLite experiment reloaded every table of the camera export into fresh temporary databases with the exporter's synchronous/journal settings. Source rows were materialized before timing; three trials per strategy included schema creation, insertion, index creation, and commit:

| Strategy | Trial times | Median |
|---|---|---:|
| Indexes before inserts | 1.645, 1.603, 1.719 s | 1.645 s |
| Indexes after inserts | 1.261, 1.392, 1.325 s | 1.325 s |

Deferred indexes reduced reload time by approximately 19.4%. This is evidence for the mechanism, not a measured 19.4% improvement to the Rust exporter: Python uses `executemany`, and the Rust exporter also generates labels, selects rows, and sorts graph edges. Production export took only about 1.5 s here, so the likely end-to-end benefit is modest.

### 4. Reduce repeated header declaration and type merging

**Where:** `crates/trace-parse/src/lower.rs`, `lower_prepared_source`; `crates/trace-parse/src/merge.rs`, `merge_types`.

Shared header IR avoids repeated body parsing, but each consumer still merges header symbols/types into a fresh per-TU `Program`. `merge_types` walks the source type table, merges aggregate declarations/layouts, and registers aliases. The completed TU is then remapped into the final program. This repeats work and creates temporary declarations for common headers.

First measure counts of imported header symbols/types and time spent importing versus AST lowering and final merge. If imports dominate, consider caching immutable declaration representations and remap plans, keyed by header variant and relevant type/layout state. A larger redesign could let a TU borrow a read-only declaration environment and own only its additions. Both require care with TU-local IDs, overloads, internal linkage, language-specific variants, anonymous classes, and dependency declarations. Do not cache remaps solely by header path.

### 5. Reduce solver allocation and reverse-index volume

**Where:** `crates/trace-analysis/src/solver.rs`, `SolverState`, `touch_loc_holders`, propagation helpers, `apply_store_to_targets`, and `analyze_with_options`.

Several smaller opportunities are explicit in the code:

- `analyze_with_options` clones all call edges and the argument-wiring set before extracting argument flow. Have extraction return its output vector, or pass disjoint field borrows, to remove both copies.
- Propagation helpers perform `contains` followed by a second insertion pass. Insert once and collect only successful insertions while borrowing disjoint state fields. This also deduplicates duplicate candidates within one input sequence.
- `loc_nodes` stores every holder of every points-to location. `touch_loc_holders` clones the holder set and then ignores nodes without Load constraints. Track Load subscribers specifically, registering existing pointees when a Load constraint is added dynamically. GEP memory dependencies need a separate semantic audit before broadening or changing subscription behavior.
- `apply_store_to_targets(None)` scans the entire source set for every target, allocates a filtered vector for each target, and reinserts old facts whenever the source grows. Pass source deltas for existing targets, while still replaying the full source set for newly discovered targets. Preserve the source's storage location, signature guards, summary updates, and all requeue requirements.

These may matter much more on callback-heavy corpora such as HDF than on camera. Do not substitute the camera's 0.5 s solver time for measurements on those inputs.

### 6. Make parsing memory limits independent of CPU count

**Where:** `crates/trace-parse/src/lower.rs`, `index_in_window`, `index_pool`; `crates/trace-cli/src/main.rs`, worker default.

Batch size is four units per worker and the channel can hold one completed batch while another is being built and the consumer merges a previous batch. Backpressure limits batch count, but unit count scales with available CPUs and there is no byte bound. Large TUs also make a count bound an unreliable memory bound. Pools give each worker a 16 MiB stack; this is a virtual allocation, not necessarily 16 MiB resident per worker.

Expose or adapt the number of in-flight units independently of worker count. Consider smaller ordered batches for large units, or a budget based on estimated source/IR size. Keep merge order deterministic. The worker-count measurement shows that reducing concurrency helps, but most memory remains at four workers, so worker tuning alone will not solve the baseline footprint.

Measure the simultaneous sizes of raw source cache, expansion text/LineMaps, retained header IR, pending TU IR, and final program. Cached expansions are self-contained and can embed nested expansions, so `Arc` sharing does not imply that each header's bytes appear only once. Raw source bytes are shared through `Arc`; cloned path maps do not duplicate those bytes. Any eviction scheme must respect headers needed by later TUs and exploration.

## Validation for follow-up changes

Implement the bounded LineMap operation and deferred secondary indexes first: both have concrete, local behavior and limited architectural impact. Address configured-mode retention next if compilation databases are a common workload.

For each change, record release elapsed time and peak RSS on camera plus HDF, and add a representative compilation-database workload for configured-mode changes. Compare every SQLite analysis table with deterministic row ordering, excluding run metadata, rather than comparing only edge counts. Run `cargo test --workspace` and the pinned corpus evaluation for semantic changes. Use existing temporary-directory helpers for regression fixtures.

Retain monotonic propagation and deterministic traversal. Lowering solver budgets or summary limits, skipping variants, omitting required flow nodes, or parallelizing mutable preprocessing discovery would change completeness or reproducibility and should not be presented as equivalent performance improvements.

## Implemented follow-up

Implemented opportunities 1 and 3:

- `LineMap::splice_range` locates the requested half-open interval and appends
  its entries directly, interning only referenced files. `append_live_chunk`
  no longer creates a temporary suffix map. The new regression test covers
  equal-offset entries, excluded boundaries, multiple original files,
  preexisting destination files, empty ranges, and ranges beyond the last entry.
- SQLite export creates tables first, loads rows, and creates all twelve
  secondary indexes before committing. A single SQL definition generates both
  the complete public `SCHEMA_V7` and the separate export phases. A regression
  test verifies primary-key/path uniqueness during loading and that phased
  creation produces the same complete schema.

Release measurements used the same corpus, options, and worker counts as their
respective baselines. The HDF baseline was measured immediately before editing;
the camera baseline is the initial review's 16-worker run. Neither measurement
below included compilation.

| Corpus | Index before → after | Analyze before → after | Export before → after | Elapsed before → after | Peak RSS before → after |
|---|---|---|---|---|---|
| Camera, 16 workers | 12.9 → 13.4 s | 0.5 → 0.5 s | 1.5 → 1.3 s | 15.49 → 16.04 s | 1,472,376 → 1,471,772 KiB |
| HDF, 8 workers | 5.9 → 5.8 s | 3.7 → 3.5 s | 1.2 → 0.8 s | 11.29 → 10.65 s | 578,040 → 575,540 KiB |

These are single-run observations with rounded stage timings. Export improved
on both inputs, but there is no established overall camera speedup or meaningful
peak-memory improvement. The LineMap change removes unnecessary interval-copy
work; these corpora do not demonstrate a large end-to-end benefit from it.
Reducing configured-mode IR retention remains a separate follow-up.

Validation: all 797 workspace tests passed; formatting and diff checks passed.
For both camera and HDF, the before/after SQLite schemas matched exactly and
SHA-256 hashes of sorted rows matched for all 12 analysis tables, excluding
`analysis_run` metadata.
The pinned HDF, hiview, and camera corpus evaluation also passed all 93 checks
with no revision or dirty-check overrides.

## Normal-path memory follow-up

Compilation-database mode was left unchanged. The normal indexing path now:

- Spills preprocessed text and LineMap entries to automatically cleaned
  temporary files once discovery and settling are done, for the sources that
  are read back: units the settle pass rebuilds are spilled by that pass only,
  and warmed headers that are not indexed on their own drop their text without
  a file. Payloads above 512 KiB are spilled; the threshold counts mapping
  capacity, since truncating nested-include mappings can retain large
  allocations with few live entries. Only live entries are written to disk,
  as one block per file.
- Keeps original paths, header provenance, language, variant selection,
  diagnostics, and conditionals resident. Reloading does not preprocess again
  or change cached expansion selection. Loaded payloads are not promoted back
  into a corpus-wide resident cache.
- Retains temporary paths rather than open file handles. Readers open separate
  cursors; eviction or dropping the cache deletes the files. Spill/load errors
  abort the normal build rather than allow publication of incomplete results.
- Replaces fixed parse batches with a window: workers take units in order
  and run at most two units per worker, between 4 and 32 in total, ahead of
  the ordered merge. Worker count and merge order are unchanged, the pending
  unit count is bounded by the window, and a slow unit holds up the merge
  rather than the workers. A panic on either side cancels the window and
  wakes every waiter, so it unwinds through the pool as the batches did.

| Corpus | Elapsed before → after | Index before → after | Peak RSS before → after |
|---|---|---|---|
| Camera, 16 workers | 16.04 → 16.81 s | 13.4 → 14.4 s | 1,471,772 → 1,282,528 KiB |
| HDF, 8 workers | 10.65 → 10.73 s | 5.8 → 6.3 s | 575,540 → 517,660 KiB |

The baseline here includes the earlier LineMap and SQLite optimizations.
These single-run observations show approximately 13% / 185 MiB lower camera
peak RSS and 10% / 57 MiB lower HDF peak RSS. Camera elapsed time increased
approximately 5%; HDF elapsed time was essentially unchanged. They do not
establish a throughput improvement. A preliminary camera run with spilling
alone (before the mapping-capacity threshold adjustment) used 1,394,872 KiB
peak RSS, suggesting that both payload retention and pending TU IR matter.

During the final camera run, one snapshot showed 392 spill files occupying
67 MiB; this was not a peak-disk measurement. Whole-command filesystem output
rose from approximately 109 MiB to 323 MiB. Temporary storage may use tmpfs,
so lower process RSS does not mean all those bytes disappear from system RAM;
file-cache pages can be reclaimed. Raw source cache, expansion cache, header
declarations/IR, final merged Program, and active parse trees remain in memory.

All 800 workspace tests passed. New tests cover exact source/LineMap/metadata
reloads, concurrent readers, source-file changes after spilling, cleanup,
truncated spill-file errors, and reserved mapping capacity. Camera and HDF
schemas and sorted-row hashes matched the preceding implementation for all
12 analysis tables, excluding run metadata.
The pinned HDF, hiview, and camera evaluation passed all 93 checks after these
memory changes, without revision or dirty-check overrides.

## Clang source benchmark: function-ID reassignment

A source-only Clang workload at LLVM revision
`404d35af4d588e03941c8fe524f881749059edcb` exposed another indexing hotspot:
`lower::reassign_fn_id`. Both CPU samples of the original `perf` revision
(`4a54a04`) put this function first among active leaf functions. Whenever a
prototype and definition shared a function ID, it scanned all variables and
call sites already imported or lowered, then searched all functions for the
surviving declaration. Repeated declarations made these scans increasingly
expensive as a TU grew.

Each of the three callers now records the variable and call-site lengths
immediately after allocating the provisional function ID. Reassignment visits
only entries appended since that point: older entities cannot refer to this
fresh ID. It still checks ownership within that suffix, preserving nested
entities with different owners, and preserves parameter order. The surviving
function is found through the existing `function_index` lookup. This changes
neither header import order nor exported IDs. A CPU sample of the optimized
run no longer showed reassignment among the leading leaf functions; descriptor
hashing/comparison, allocation, and header merging remain substantial costs.

### Reproduction and scope

Use a sparse checkout of the public LLVM repository, pinned to the revision
above, containing `clang/lib`, `clang/include`, and `llvm/include`:

```sh
git clone --depth 1 --filter=blob:none --sparse https://github.com/llvm/llvm-project.git /tmp/pathologist-llvm
git -C /tmp/pathologist-llvm fetch --depth 1 origin 404d35af4d588e03941c8fe524f881749059edcb
git -C /tmp/pathologist-llvm checkout --detach 404d35af4d588e03941c8fe524f881749059edcb
git -C /tmp/pathologist-llvm sparse-checkout set clang/lib clang/include llvm/include
cargo build -p trace-cli --release
cargo run -p trace-cli --release -- analyze /tmp/pathologist-llvm/clang \
  --include /tmp/pathologist-llvm/clang/include \
  --include /tmp/pathologist-llvm/llvm/include \
  --jobs 8 --timeout-secs 600 -o /tmp/clang.db
```

This discovers 1,124 TUs and 1,386 headers, with 2,316 cached expansions of
1,059 headers. It is not a configured LLVM build: generated `.inc` files and
C++ standard-library headers are absent. The baseline emits 5,183 preprocessor
diagnostics and 990 parse diagnostics. These results measure performance on
this input, not complete semantic coverage of Clang. Keep the input paths and
options identical when comparing database rows.

### Measurements

Measured on macOS 26.6.2 arm64 with Rust 1.100.0-nightly
(`bff8e12ff`, 2026-08-26), release builds, eight workers, minimal export.
The isolated original run and optimized run used the same checkout and paths:

| Measurement | Original `perf` | Optimized |
|---|---:|---:|
| Preprocess discovery + settle | 44.4 s | 41.9 s |
| Header IR construction (`pch`) | 62.9 s | 59.7 s |
| Cumulative indexing (`index`) | 366.3 s | 306.3 s |
| Whole command | 372.2 s | 312.1 s |
| User CPU time | 1,528.7 s | 1,217.5 s |
| Peak RSS | 1.78 GiB | 2.21 GiB |

The CLI's `index` time includes the preprocessing and PCH phases above; these
rows must not be added together. Whole-command time fell 16.1%, cumulative
indexing 16.4%, and user CPU time 20.4%. Peak RSS was 24% higher in this pair;
this is a speed improvement, not evidence of a further memory reduction.
The patch adds no persistent storage. Repeated runs (see the next section)
show peak RSS varying by more than 0.8 GiB between identical runs on this
8 GiB host, so the difference in this pair is within that variation.

Timing and peak RSS came from Python `time.monotonic()` and
`resource.getrusage(RUSAGE_CHILDREN)` in a fresh runner process per command
(`ru_maxrss` is bytes on macOS). Both binaries were built before timing; the
optimized command used `cargo run --release`, while the isolated original ran
a saved release binary built from `4a54a04`. Cargo startup was under one second.
No builds or tests overlapped these two runs. An earlier original run, partly
overlapping compilation/testing, took 370.5 s total / 364.9 s indexing and is
excluded from the comparison. CPU samples are short observations, not a
whole-run attribution of time. These are individual runs, not statistical
confidence intervals.

### Correctness checks

The original and optimized Clang exports match across every row of all 12
analysis tables (4,097,781 rows), excluding only `analysis_run` metadata.
This includes diagnostics, not just aggregate function/edge counts. Both
exports contain 163,395 functions, 506,120 call edges, and 140,575 argument-flow
edges. Workspace tests pass (817 tests), Clippy is clean, and the pinned
OpenHarmony evaluation passes all 93 checks. An independent review checked
all three provisional-ID allocation and reassignment callsites.

## Clang source benchmark: header import and lowering

Same Clang workload, checkout and command as above, measured after the
function-ID change (`22eda29`). Stage timers summed across the eight workers
and a ten-second `sample` of the indexing phase gave the following picture:

- Every translation unit merged 272 header units on average (376,344 merges
  for 1,386 units). That import took 516 of roughly 990 worker CPU seconds in
  indexing, and 52 of 61 in header IR construction. Lowering took 314 s and
  tree-sitter parsing 160 s. The serial merge took 28 s, and the merge thread
  waited most of the time, so the workers, not the merge, were the bottleneck.
- Inside the import, the time went to hashing and comparing descriptor trees
  that the receiving table already held, and to cloning complete class
  descriptors when a pointer to an empty named tag was canonicalized.
- During lowering, every name lookup with internal-linkage scope walked the
  unit's whole included-header set (hundreds of files per Clang unit).

The changes, in the order they were measured:

1. `TypeTable::intern_arc` answers by address (`by_ptr`) for a descriptor
   allocation the table already holds. Descriptors are shared `Arc`s, so a
   header re-merged through several includers presents the same allocations.
   Import fell from 516 to 199 s in indexing and from 52 to 22 s in PCH.
2. A header unit precomputes the descriptor each of its types merges as
   (`UnitIndex::merge_descs`): its own, or the aggregate rebuilt from its
   layout. Consumers intern those by address instead of rebuilding. Only
   header units pay for it; a translation unit merges once.
3. A rewritable descriptor (a `Ptr` / `Array` chain to an empty named tag)
   is answered from `CanonicalCache`, keyed by that small spelling and
   validated against the tag's current id, the tag's descriptor allocation
   and an aggregate-union epoch, so it follows tag completion exactly. This
   also serves lowering, which spells `this` and receivers that way. Import
   fell to 148 s and lowering from 284 to 215 s.
4. `SymbolTable` indexes internal-linkage functions and file statics by name
   and filters the entries by the asking file's scope, with the same
   nearest-first order as before, instead of probing a per-file map for every
   file in the scope.
5. Template base facts are kept in a set beside the list and re-added by
   reference, since every consumer of a header re-adds the header's facts.

### Measurements

Same machine and options as the previous section, release builds, eight
workers, the two binaries run alternately, two runs each. Every one of the 12
analysis tables matched row for row between the two binaries in both pairs.

| Measurement | `22eda29` run 1 | This change run 1 | `22eda29` run 2 | This change run 2 |
|---|---:|---:|---:|---:|
| Preprocess discovery + settle | 39.3 s | 40.4 s | 42.1 s | 50.3 s |
| Header IR construction (`pch`) | 58.2 s | 24.0 s | 58.3 s | 25.6 s |
| Cumulative indexing (`index`) | 281.9 s | 195.7 s | 314.9 s | 227.1 s |
| Whole command | 286.7 s | 200.7 s | 325.9 s | 232.1 s |
| User CPU time | 1,194.9 s | 829.8 s | 1,259.8 s | 868.7 s |
| Peak RSS | 3.28 GiB | 3.23 GiB | 2.44 GiB | 2.95 GiB |

Whole-command time fell 30% and 29%, header IR construction 58%, and user CPU
31%. Peak RSS on this 8 GiB host varies by more than 0.8 GiB between identical
runs (the two baseline runs differ by that much), so no memory conclusion is
drawn from this pair. The new per-table caches hold one map entry per
interned type and one small key per rewritable spelling; the per-unit merge
descriptors hold one `Arc` per type of each header unit.

### Two findings left for separate work

**Cached headers are less complete than inlined ones.** After settling, 910 of
the 1,124 units still inline 36,058 headers into their own text: the
preprocessed unit texts total 677 MB against 46 MB of `.cpp` source, so most of
the parsing and lowering time is header code processed once per unit. The
cause is `max_expansion_variants`: with the default of 8, 301 headers have
their variant list full, and a unit whose macro environment matches none of
the stored variants expands the header itself. Raising the cap to 64 for one
run left 38 headers at the cap, cut unit text to 226 MB and user CPU to 536 s,
but changed the output: 160,008 functions instead of 163,395 and 4,186 files
instead of 4,251, with matching differences in call and flow edges. So a
header replayed from the cache and lowered as a header unit contributes less
than the same header inlined into the unit, and the cap is not a free knob.
Closing that gap would make the cache, and a higher cap, the largest remaining
win on this workload.

**A local class marks its enclosing function virtual.** `virtual_flags` walks
a definition's whole subtree, body included, so a function that defines a
local class with virtual members is recorded as virtual and dispatches to
overrides. Skipping the body changes one call edge and three argument-flow
edges on Clang. Correct, but a behavior change, so it is not part of this
performance change.

## Weak symbols and link-target scoping (#106)

Measured on 2026-09-16 against `2b61ae4`, using release builds, minimal export,
`--jobs 8`, and ten alternating before/after runs per corpus. HDF was pinned at
`cdc75a20bb8f1a046cd22e189405a20d602d0521`; camera at
`8ffd69dcd47f533e70b4dba428439da9008b0cae`. These corpora exercise the ordinary
path without link metadata. Values below are medians over all ten runs, with
the observed range in brackets; memory is MiB as reported by macOS
`/usr/bin/time -l`.

| Corpus | Elapsed before → after | Peak RSS before → after | Peak footprint before → after |
|--------|------------------------|-------------------------|-------------------------------|
| HDF | 3.38 → 3.35 s [3.29–3.72 / 3.33–3.45] | 362.7 → 367.9 [338–378 / 360–379] | 297.0 → 298.4 [288–328 / 271–332] |
| Camera | 4.78 → 4.76 s [4.73–4.85 / 4.66–4.89] | 665.4 → 668.3 [641–688 / 647–737] | 740.9 → 703.8 [708–750 / 656–761] |

Neither runtime nor memory shows an effect in either direction. Both elapsed
medians move by under 1% with before-ranges that straddle the after-medians,
and memory readings sit inside heavily overlapping ranges: peak RSS differs by +1.4% on HDF and +0.4%
on camera, peak footprint by +0.5% and −5%. Sample size matters here — a
five-run subset of the same data showed peak RSS up 2.5–3%, which ten runs did
not reproduce, so RSS differences of a few percent on this workload should be
read as noise rather than signal. The first invocation of a newly built binary
is consistently the slowest of its session. These measurements do not establish
performance for every build.

All 12 pre-existing analysis tables matched exactly on their original columns
for both corpora, excluding `analysis_run` metadata: identical row counts and
identical row sets for `call_edges`, `arg_flow_edges`, `call_sites`,
`flow_nodes`, `flow_edges`, `functions`, `variables`, `files` and
`diagnostics`; the remaining three (`types`, `locations`, `points_to`) are
empty under minimal export, so they match trivially. Only the three new target
tables are added. No evaluation
expectations needed updating. New weak/target columns were excluded from that
comparison.

The implementation preserves the ordinary indexing path when no link metadata
exists. Weak annotation scans skip units without weak symbols, imported weak
presence uses a monotonic symbol-table cache, and weak body/initializer ownership
ranges are allocated only for link-aware indexing. Compilation commands are
parsed once and object/configuration membership reuses that result. With link
metadata, each configured unit is lowered once; target-specific IR instances
are then merged separately. Their additional memory represents distinct target
bindings, while type descriptors remain shared. Signature selection considers
only names with weak definitions and indexes parameter types once per unit.

Two properties bound the link-aware path's cost. Scoping rewrites a full copy
of each unit it merges, so `merge_target` builds and merges those copies one at
a time (`VariantMerge`, the streaming form of `merge_unit_variants`) rather than
materializing the family first: peak memory carries one scoped unit, not a
second copy of everything a target links. And a multi-config CMake reply lists
each target once per configuration over identical sources, so only the first
configuration is read; indexing all of them would multiply the whole corpus by
the configuration count for no additional facts.

## Analyze-phase performance (#117)

Profiles, the alternating A/B measurements and the output-equivalence results
are recorded in the
[evaluation report](EVAL_REPORT.md#analyze-phase-performance--2026-09-21-117).
What the solver does about allocation and store filtering is defined in
[Propagation highlights](ANALYSIS.md#propagation-highlights); name-lookup
ordering is in [Shared header functions](ANALYSIS.md#shared-header-functions).

## Incremental per-TU IR cache: macOS measurements and decision (#175)

Issue #175 asked whether a ccache-like cache of lowered per-TU `UnitIndex`
values could reduce **peak memory** on a re-index after a partial change,
gated on measurements before any implementation. This section records the
Gate 0–3 findings on macOS (one of the two primary targets), the decision,
and what the measurements say the peak is actually made of. **Windows was not
measured**; nothing here claims a Windows result.

Setup: repository `a284120`, release profile (thin LTO, one codegen unit),
default system allocator unless stated, `analyze ~/ability_ability_runtime
--jobs 8`, minimal export, default solver budgets. Corpus
`ability_ability_runtime` at `6c18fdc9bdef6cfcf5888517cd8ed9448584f6e8`
(clean; 3,331 TUs, 2,992 headers, 6,344 files in the include graph, 49.4 MiB
of source text). Machine: Apple M1 (4 performance + 4 efficiency cores),
8 GB RAM, macOS 26.6.2 arm64, rustc 1.100.0-nightly (bff8e12ff 2026-08-26).
Memory was sampled every 100 ms with
[`scripts/profile_memory_macos.py`](../scripts/profile_memory_macos.py)
(`proc_pid_rusage`: resident size, `phys_footprint`, the kernel's lifetime
maximum footprint, CPU time), attributed to phases by the analyzer's stderr
lines, with `ru_maxrss` from `wait4`. Live allocator bytes came from a scratch
build that printed `malloc_zone_statistics` at phase boundaries and from a
100 ms in-process sampler of the same counter; those probes are not in the
tree.

### What "peak" means on macOS

`ru_maxrss` and sampled RSS **understate** demand here: with 8 GB the kernel
compresses pages under pressure, and compressed pages leave RSS but stay in
`phys_footprint`. Across fifteen system-allocator runs (instrumented ones
included) the lifetime maximum footprint ranged **1,758–2,408 MiB** (median
2,155 MiB) while `ru_maxrss` ranged 1,217–1,551 MiB. Footprint is the number
the kernel acts on and the one that lines up with the Linux `ru_maxrss` of
≈1.81 GiB in
[EVAL_REPORT.md](EVAL_REPORT.md#compacting-cached-header-type-tables--2026-09-28);
all peaks below are footprint unless labelled otherwise. The run-to-run spread
(±15%) comes from discovery scheduling (which unit pioneers a variant) and
from when the kernel reclaims freed pages, not from live data: identical
output every run.

The macOS allocator (`DefaultMallocZone` on macOS 26) returns freed pages
eagerly with `MADV_FREE_REUSABLE`: after indexing on the camera corpus it
reported 763 MiB "allocated" against 127 MiB live while the footprint was
≈262 MiB, and `malloc_zone_pressure_relief(NULL, 0)` released **0 bytes** at
every phase boundary in three instrumented runs. So the Linux `malloc_trim`
lever has no macOS counterpart worth adding, and the high resident numbers
are reusable pages the kernel reclaims lazily. `mimalloc` (`--features
mimalloc,mimalloc/override`) is a trade: 32.8–34.4 s wall and 122–123 s CPU
against 35.3–36.6 s and 143–147 s, but a peak footprint **about 20%
higher** (2,310–2,382 MiB versus 1,749–2,131 MiB in the alternating pairs
below).

### Gate 1 — the cold-run peak and the warm-run floor

Three alternating pairs, phase peak footprint in MiB, measured with the
script as committed (the `warm` phase is the sequential header warming,
whose peak sits below the include-graph transient):

| Run | Wall | CPU | `ru_maxrss` | Max footprint | graph | warm | preprocess | pch | TU merge | analyze | export |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| system 1 | 36.6 s | 143 s | 1,409 | 1,749 | 950 | 704 | 1,586 | 1,702 | 1,749 | 1,090 | 1,097 |
| mimalloc 1 | 33.2 s | 123 s | 1,682 | 2,382 | 967 | 969 | 2,327 | 2,327 | 2,373 | 2,172 | 1,128 |
| system 2 | 35.3 s | 144 s | 1,611 | 2,106 | 1,021 | 744 | 1,681 | 1,816 | 2,081 | 2,100 | 870 |
| mimalloc 2 | 34.4 s | 122 s | 1,382 | 2,328 | 843 | 854 | 2,305 | 2,262 | 2,326 | 2,164 | 1,150 |
| system 3 | 36.5 s | 147 s | 1,493 | 2,131 | 999 | 704 | 1,590 | 1,737 | 2,101 | 2,103 | 882 |
| mimalloc 3 | 32.8 s | 123 s | 1,665 | 2,310 | 968 | 832 | 2,270 | 2,249 | 2,310 | 2,022 | 1,286 |

Live allocator bytes tell a different story from the footprint. At phase
boundaries (one run): 85 MiB after the include graph, 1,033 MiB after
preprocessing, 806 MiB after header IR, **645 MiB after `index` returns**
(the merged `Program`, caches dropped), 810 MiB after analysis and export.
Sampled every 100 ms, the live peaks were: include graph **760 MiB** at
t = 2.0 s, warm 521 MiB, **preprocessing 1,357 MiB** at t = 10.6 s, header IR
1,181 MiB, TU merge 1,262 MiB, analyze 814 MiB. So the live peak is
discovery, not the TU merge; the TU-merge footprint peak carries 0.6–1.1 GiB
of freed pages the kernel has not reclaimed yet, and the include-graph phase
burns a 760 MiB transient that leaves 85 MiB behind.

The warm-run floor is therefore max(include-graph transient ≈ 0.95–1.05 GiB
footprint, `Program` + analysis ≈ 0.86–1.03 GiB) ≈ **1.0 GiB, 45–55% of the
cold peak** — under the 80% no-go line, but it caps the best case of any
cache at roughly the ≤ 60% acceptance bar before a single changed TU is
processed.

**Closure experiment.** A scratch build kept every *k*-th TU in index order
before warming (`TRACE_RESEARCH_TU_STRIDE`), treating headers reachable only
from dropped TUs as covered by hypothetical cached IR (excluded from the
orphan path). This models the best case where warm, discovery and header-IR
selection are all driven by the changed subset — which the code as written
does not do (all three derive from the full TU set, `lower.rs` 629–659,
825–833, 884–970). Phase peak footprint, two runs each:

| TUs kept | Closure files | Header IR | preprocess | pch | TU merge | Process peak | Wall |
|---|---:|---:|---:|---:|---:|---:|---:|
| 334 (10%) | 1,440 | 1,054 headers / 1,866 expansions | 778–824 | 846–899 | 931–940 | **983–986** (= include graph) | 8.2–8.5 s |
| 1,666 (50%) | 3,632 | 1,894 / 3,489 | 1,227–1,293 | 1,389–1,414 | 1,441–1,578 | 1,485–1,579 | 19.7 s |
| 3,331 (100%) | 6,344 | 2,410 / 4,511 | 1,543–1,735 | 1,606–1,892 | 1,945–2,255 | 1,946–2,381 | 33–36 s |

Header IR and the expansion cache do shrink with the closure of the
preprocessed subset (that part of Gate 1 passes), and at 10% the process
peak is set entirely by the include-graph phase, which is run-wide. The
reduced-TU rows merge a smaller `Program`, so they are not a partially warm
full-corpus run; the full-corpus floor above is the number to add back.

### Gate 2 — the key cannot be computed without preprocessing

On the unconfigured path — the one this corpus takes, since its GN metadata
is incomplete and no symbol carries a `target_id` — a TU's lowered IR is not
a function of its own bytes, include closure and flags:

- `matching_variant` takes the **first** stored variant whose
  `MacroFingerprint` the environment satisfies
  (`crates/trace-preproc/src/preprocessor.rs` 1421–1437); `publish_variant`
  appends only while fewer than `max_expansion_variants` (8, never
  overridden) are stored (1386–1415). A ninth environment gets
  `Placed::Held`: the header's declarations land in that TU's own unit
  (`inlined_headers`), and no header unit exists for it. Renaming an earlier
  TU so it sorts after this one flips that outcome — same bytes, same
  closure, different `UnitIndex`.
- Guard reads are deliberately not charged to fingerprints
  (`guard_suppresses`, 875–898; charging them cost camera 879 edges), so two
  entries can satisfy one environment yet differ in `nested_variants`,
  `guards` and `ops`; the first publisher wins. The tree records this
  class: with racing publication camera produced 20,299 / 20,326 / 20,847
  direct edges in three runs of one checkout
  ([PREPROCESSOR.md](PREPROCESSOR.md#parallel-discovery-88)).
- A TU with no record for a header merges **every** lowered variant of it
  (`lower.rs` 2593–2597); header language follows reachability from all C++
  TUs (`cpp_parse`, 755–760); the order of header-origin entities in a unit
  follows the global `pch_order` rank (2164–2172, a topological order over
  the whole tree's `pch_headers`), and entities first introduced by a TU
  take ids in that order (`merge.rs` 684, 945) — so the same unit lowered
  under a different tree state can shift merged ids and export rowids.
- Binding hashes include the defining spelling's file, line and column
  (`hash_macro_binding`, 4956–4979): moving a `#define` by one line
  invalidates every fingerprint that read it.

A correct key must therefore include the settled preprocess output, the
identities and contents of the header variants the unit merged, the
reachable header set with its `HeaderOrder` ranks, dependency and system
roots, the test partition, ignored macros (including `--models` noise
macros), explore candidates and the tool version — a "preprocessor-mode"
key that exists only after discovery and settle have run for that TU.
Skipping preprocessing would require persisting each unit's
`ExpansionJournal` and re-validating it with `ExpansionJournal::agrees` at
its commit turn; the in-memory protocol supports that check, but
`IncludeExpansion::id` is a process-local counter, entries hold
`Arc<LineMap>`, macro token vectors and a `MacroHistory` undo log, and
nothing serializes them. Its feasibility is not determined and is not
small. The configured path (compilation database or scoped link targets)
inlines every header and touches no shared cache
(`configured.rs` 69–134), so a direct-mode key is plausible there, with the
usual shadowing hazard (a new file resolving earlier via the basename
fallback or a new inferred include directory).

Consequence for memory: on the unconfigured path a warm run still runs
warm, discovery and settle for **every** TU, so the preprocessing phase keeps
its 1.5–1.7 GiB footprint peak (≈ 75–80% of cold) and its 1.36 GiB live peak.
The memory objective fails at this gate.

### Gate 3 — per-TU IR is serializable but embeds its closure

`UnitIndex` holds unit-local ids remapped at merge, no raw pointers, `Rc`,
tree-sitter nodes or `Arc<dyn>`; `merge_descs` is a rebuildable cache;
merge does not depend on how a unit was produced (`merge.rs` 325–779,
`program.rs` 194–240); `CallReturn` expands in `pag.rs`, and lowering's
`resolve_function*` calls query the per-unit `Program` only. Two blockers:
`TypeTable` has no serde and holds a process-global `DescriptorPool` handle,
an address-keyed map and a `CanonicalCache` (needs a re-interning
deserializer), and `program_into_unit` (`lower.rs` 2885–2923) moves the whole
per-unit `Program` — every reachable header's types, prototypes, globals and
internal C++ bodies — into the TU's unit. A naive per-TU cache duplicates
each closure on disk; caching per `(header, language, variant)` plus the TU's
own part is the only sane shape, and it inherits the Gate 2 key.

### Gate 0 — workloads

CI runs on fresh checkouts (cold). The eval loop (`scripts/eval_check.py`)
re-runs each corpus with a *different binary*, so a lowering-crate
fingerprint in the key invalidates everything unless only `trace-analysis`
/ `trace-db` changed (an *f* = 0 case, not a small-*f* one). `--explore`
re-preprocesses within one run. The C API's `trace_index` runs the whole
pipeline per call; a long-lived host or a developer's edit-and-rerun loop is
the only small-*f* consumer, and no such host exists in the tree.

### Decision: no-go for the cache as specified

- Memory: fails Gate 2 on the unconfigured path (preprocessing must run for
  every TU; 75–80% of cold peak remains) and would, even with discovery
  solved, bottom out at the ≈ 1.0 GiB include-graph / `Program` floor
  (45–55% of cold), right at the ≤ 60% bar with nothing left for the changed
  TUs' closure.
- Time: a hit skips parse + lower + closure re-merge (≈ 16 s of the 29 s
  index phase, ≈ 55%) but not discovery, warm, header IR or merge; a fully
  warm run lands near 50–55% of cold against the ≤ 40% criterion.
- Not measured: Windows. A configured-path-only cache (direct-mode key) is
  a separate, narrower proposal and needs its own numbers.

The three approaches raised during review map onto these findings: caching
header IR per variant is the right *unit* but inherits the Gate 2 key;
persisting discovery state is the crux and is undetermined; streaming cached
units into the ordered merge is already how the window works (4–32 units
ahead of the merge), and the floor it would stream into is the problem.

### What the peak is made of, and where the time goes

These are cold-run levers; they are what the measurements point at, and
they lower any future warm run's floor too.

Per-phase wall, CPU and average busy cores (one run):

| Phase | Wall | CPU | Cores | Notes |
|---|---:|---:|---:|---|
| include graph | 2.3 s | 12.5 s | 5.6 | 760 MiB live transient, 85 MiB retained |
| warm | 2.0 s | 2.0 s | 1.0 | sequential by design |
| preprocess | 6.3 s | 36.2 s | 5.8 | 1,299 of 3,331 units preprocessed twice (settle) |
| header IR (pch) | 2.1 s | 6.5 s | 3.1 | 2,410 headers, 4,511 expansions |
| TU parse / lower / merge | 15.6 s | 68.5 s | 4.4 | 52% of all CPU |
| analyze | 0.9 s | 0.9 s | 1.0 | |
| export | 2.9 s | 2.7 s | 1.0 | |

`sample` call-stack profiles (1 ms, all threads) of the busy worker time:

1. **TU phase, 33%: re-merging the header closure into a fresh per-unit
   `Program`.** Each TU merges the symbols of every reachable header unit
   (`lower.rs` 2581–2606): 3,331 TUs perform **328,588 header-unit merges**
   (median 32, mean 99, max 528 per TU), re-interning **31.7 M** type
   entries of which 1.16 M survive — each type is merged about **27×**
   overall (a median TU meets each of its types 8×, the largest 64×) —
   because a header unit carries its nested closure's types again.
   Functions are a smaller story: header units carry only their own
   prototypes, so 10.2 M declarations merge to 4.2 M kept (2.4×,
   concentrated in large closures; the median TU merges no duplicate). The
   per-unit program ends with a median of 377 functions and 184 types
   (means 1,269 and 349); the cost is the dedup itself
   (`merge_types` → `intern_ref`/`intern_shared` with `TypeDesc` hash,
   `compute_layout` per fresh table, `merge_struct_declarations`;
   `register_function_inner`, `MergeDedup::insert_fn`). Exact-closure
   memoization would save only 7% (2,935 distinct closures among 3,331 TUs);
   not re-merging imported entries is the lever: header units recording,
   for each nested unit merged at their own lowering, the remap from that
   unit's ids to theirs, so a TU composes remaps for imported entries and
   interns only a header's own additions. That is the "immutable
   declaration environment with per-unit additions/remaps" of
   [MEMORY_PROFILE.md](MEMORY_PROFILE.md#changes-with-the-greatest-potential),
   and it shrinks header IR as well (type tables were 93% of it).
2. **TU phase, 24% tree-sitter parse, 22% `lower_tree`; malloc/free show up
   in ≈ 18% of busy samples across all of it.**
   The coordinator's merge (`VariantMerge::start`) is busy 31% of the phase
   and waiting otherwise; the workers, not the ordered merge, bound the
   phase at 4.4 of 8 cores.
3. **Preprocessing: `process_file` is 86% of busy time**, the lexer 14%,
   malloc/free ≈ 18%; discovery holds the live peak (1,357 MiB: expansion
   cache entries with their `ops`, diagnostics and line maps, plus
   journals and per-unit texts in flight).
4. **Include graph: `is_file_cached` is 40% of the phase** and is dominated
   by `realloc`/`RawVec::finish_grow` growing the per-thread probe caches
   keyed by full candidate paths; the allocator's own bookkeeping on `free`
   (which reads `mach_absolute_time`) is another ≈ 10% of the phase's busy
   samples, so the churn costs twice. This is the 760 MiB transient and the floor of any
   warm run; camera already showed 1.1 M probe entries and 117 MiB of path
   keys ([MEMORY_PROFILE.md](MEMORY_PROFILE.md#what-occupies-memory)).
   Since #209 the memo records only answers the filesystem gave, and the
   graph scan shares one walk per spelling; see
   [LLVM monorepo startup peak](MEMORY_PROFILE.md#llvm-monorepo-startup-peak-209).
5. Sequential tails: warm (2.0 s), export (2.9 s) and analyze (0.9 s) run on
   one core — 17% of wall.

Reproduce: `python3 scripts/profile_memory_macos.py --label base --out
/tmp/mem -- target/release/trace analyze ~/ability_ability_runtime --jobs 8
-o /tmp/mem/base.db`; the script prints the child's pid at launch, so
`sample <pid> 10 1 -mayDie` (with `sudo` unless the machine allows
unprivileged process inspection) can be started during the phase of
interest. Compare only within one platform and allocator.
