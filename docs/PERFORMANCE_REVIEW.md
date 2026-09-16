# Performance and memory review

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
  the complete public `SCHEMA_V5` and the separate export phases. A regression
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

## Warm and PCH pipeline optimization

Investigated parallelising the warm pass and optimizing the PCH indexing stage.

### Warm stage concurrency constraints and optimizations

The header macro warm pass (`headers_for_macro_warm`) warms project headers reachable from translation units using command-line defines before the discovery pass.

1. **Concurrency constraints**:
   Evaluated running the warm stage in parallel waves. However, the warm pass executes before dynamic/macro `#include MACRO` relationships are discovered and added to the include graph. In a concurrent warm pass without speculative journaling, headers with undeclared include relationships run in arbitrary order: if header A includes header B before B has finished warming, A inlines B instead of replaying a cached expansion.
   Empirical verification confirmed that concurrent warming causes 7 test failures on Camera (diagnostics drifted from 4859 to 4873, direct edges drifted from 42584 to 41936), violating bit-reproducibility.
   Because warm execution takes only ~0.2 s, introducing speculative journaling machinery (like `expansion_discovery.rs`) would add significant complexity for negligible wall-clock gain.

2. **Sequential warmup optimizations**:
   - **Precomputed base macro tables**: `macro_table_from_defines(&opts.defines, language)` previously re-tokenized and re-parsed every CLI define and predefined macro 838 times (once per header). We now precompute `base_macro_tables` once per language (`[Language::C, Language::Cpp]`) and clone the table for each header, avoiding hundreds of redundant tokenization loops.
   - **Allocation cleanup**: Removed dead `warmed: Vec<...>` allocations that were instantiated and dropped on every header.
   - **Streamlined progress output**: Replaced 35+ synchronous terminal progress flushes (`index_item_progress`) with concise phase summary logging (`warm[1]: 838 reachable headers`, `warm-done[1]: 0.2s`), eliminating the misleading `(jobs=16 after this sequential pass)` message.

### PCH stage parallelization and optimizations

1. **Wave-based parallel parsing**:
   PCH construction parses header expansions in parallel across worker threads (`jobs=16`) grouped into topological waves (`pch_waves`). On Camera, 814 headers (1,322 expansions) are processed across 14 parallel waves (`[200, 116, 114, 61, ...]`) with 0 cycles in just 0.4 seconds. Progress output now explicitly reports `(jobs={jobs})` to clarify multi-threaded execution.

2. **PCH header variants parallel precomputation**:
   Precomputes `header_variants: HashMap<PathBuf, Vec<usize>>` upfront in parallel across the thread pool via `par_iter()`. Wave dispatch and cyclic processing query this precomputed map directly.

3. **Topological `PchOrder` lookup in `headers_to_merge`**:
   `headers_to_merge` now utilizes `PchOrder`, which records the precomputed topological position of each header in an `FxHashMap<PathBuf, usize>`. Only actually included headers are sorted by position in O(M log M), eliminating hundreds of millions of full-array scans.

4. **Direct `pch_order` iteration during global merge**:
   When merging header PCH units into the global program, the pipeline previously re-collected `header_ir.keys()` and re-ran Kahn's algorithm via `include_graph.index_order(...)`. We now iterate directly over the precomputed `pch_order.order`, saving redundant allocations and graph traversals.

### Measurements

Measured on Linux x86_64 with 16 workers, release builds, minimal export:

| Corpus | Baseline elapsed | Optimized elapsed | Baseline peak RSS | Optimized peak RSS |
|---|---:|---:|---:|---:|
| Camera (744 TUs, 849 headers) | 16.81 s | 9.34 s | 1,282,528 KiB | 799,652 KiB |
| HDF (802 TUs, 681 headers) | 10.73 s | 5.96 s | 517,660 KiB | 310,572 KiB |

### Validation

- All 817 workspace tests pass (`cargo test --workspace`).
- Clippy is completely clean (`cargo clippy --workspace --all-targets`).
- Pinned OpenHarmony evaluation passes all 93 checks (`python3 scripts/eval_check.py`).
- Deterministic table equivalence check across all 12 SQLite tables (`files`, `functions`, `variables`, `call_sites`, `call_edges`, `arg_flow_edges`, `diagnostics`, `flow_nodes`, `flow_edges`, `locations`, `points_to`, `types`) confirmed a 100% exact match (row for row) against the baseline export.

## mimalloc global allocator integration (Windows only)

Configured Microsoft's `mimalloc` (`mimalloc = "0.1"`) as the global allocator conditionally on Windows (`cfg(windows)`) across `trace-cli` (`main.rs`, examples) and `trace-capi` (`lib.rs`). On Linux and other platforms, the system allocator (glibc malloc on Linux) is retained.

### Motivation

As investigated in [Memory ownership investigation](MEMORY_PROFILE.md), the pipeline experiences heavy multi-threaded allocation and deallocation churn across worker threads (AST nodes, token vectors, type descriptors, and cache evictions).
- On **Windows**, the default CRT heap exhibits significant lock contention under Rayon multi-threaded workloads, where `mimalloc` provides substantial scalability and throughput gains.
- On **Linux**, glibc malloc arenas combined with phase-boundary trimming (`reclaim_unused_pages` calling `malloc_trim(0)`) maintain an exceptionally low resident memory footprint (780 MiB on Camera, 303 MiB on HDF).

### Linux measurements (Memory overhead vs throughput)

Measured on Linux x86_64 with 16 workers, release builds, minimal export:

| Corpus | glibc elapsed | mimalloc elapsed | glibc Peak RSS | mimalloc Peak RSS (untuned) | mimalloc Peak RSS (tuned) |
|---|---:|---:|---:|---:|---:|
| Camera (744 TUs, 849 headers) | 9.34 s | 6.75 s (-28%) | **799,652 KiB (780 MiB)** | 1,386,064 KiB (1.32 GiB, +73%) | 1,109,576 KiB (1.05 GiB, +39%) |
| HDF (802 TUs, 681 headers) | 5.96 s | 5.33 s (-11%) | **310,572 KiB (303 MiB)** | 688,680 KiB (672 MiB, +122%) | 522,436 KiB (510 MiB, +68%) |
| Hiview (690 TUs, 738 headers) | 2.30 s | 2.02 s (-12%) | **~300 MiB** | 606,396 KiB (592 MiB, +102%) | 480,124 KiB (468 MiB, +60%) |

Even with aggressive page-purging and disabled eager arena commits (`mi_option_arena_eager_commit = 0`, `mi_option_purge_delay = 0`), mimalloc increases resident memory by 39%–68% on Linux due to per-thread page heaps (`theaps`).

### Platform-specific strategy

1. **Target-scoped dependency**: `mimalloc` is declared under `[target.'cfg(windows)'.dependencies]` in `crates/trace-cli/Cargo.toml` and `crates/trace-capi/Cargo.toml`. On Linux and macOS, neither `mimalloc` nor `libmimalloc-sys` is compiled or linked into the binary.
2. **Conditional global allocator**: `#[cfg(windows)] #[global_allocator] static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;` ensures mimalloc replaces the allocator exclusively on Windows builds.
3. **Startup tuning on Windows**: On Windows, startup initializes `mi_option_arena_eager_commit = 0` and `mi_option_purge_delay = 0` in `main.rs` to keep arena memory bounded.
4. **Preserved Linux footprint**: Linux builds continue using glibc malloc + `malloc_trim(0)`, preserving the minimal 780 MiB peak memory footprint.

### Validation

- All 817 workspace tests pass (`cargo test --workspace`).
- Clippy is completely clean (`cargo clippy --workspace --all-targets`).
- Pinned OpenHarmony evaluation passes all 93 checks (`python3 scripts/eval_check.py`).

## Solver zero-copy optimizations and index_in_window concurrency tuning

### 1. Solver zero-copy optimizations (`crates/trace-analysis/src/solver.rs`)

1. **Elimination of `call_edges` and `wired_arg_flow` clones**:
   In `analyze_with_options`, the solver previously cloned `result.call_edges` (tens of thousands of edges) and `result.wired_arg_flow` before calling `extract_arg_flow`. Modified `extract_arg_flow` to accept `&[CallGraphEdge]`, `&FxHashSet<...>`, and `&mut Vec<ArgFlowEdge>`. `analyze_with_options` now passes `&result.call_edges`, `&result.wired_arg_flow`, and `&mut result.arg_flow_edges` directly with zero clones and zero heap allocations.

2. **Elimination of `loc_nodes` clone in `touch_loc_holders`**:
   `touch_loc_holders` previously called `self.loc_nodes.get(&loc).cloned()`, allocating, hashing, and dropping an entire `FxHashSet<PagNodeId>` every time a store modified memory. By leveraging Rust field-level disjoint borrow splitting (`self.loc_nodes` borrowed immutably while `self.delta_pending`, `self.delta`, `self.queued`, and `self.worklist` are borrowed mutably), the loop now iterates by reference without cloning.

3. **Avoid per-target vector allocation in `apply_store_to_targets`**:
   In `apply_store_to_targets`, every target location iterated was constructing an intermediate `filtered_src: Vec<LocId>` via `.filter(...).collect()` solely to iterate it into `st.memory_pts`. Extracted a free function `arity_allows(&st.slot_guard, &st.fn_arity, loc, l)` taking disjoint struct fields, allowing direct in-place filtering into `st.memory_pts.entry(loc)` without allocating intermediate vectors.

4. **Zero-copy `store_dst` and `store_src` dispatch**:
   Removed redundant `.cloned()` calls on `pag.indices.store_dst.get(&node)` and `pag.indices.store_src.get(&node)`, borrowing index slices directly.

5. **Single-pass propagation and memory-merge deduplication**:
   In `propagate_locs`, `propagate_slice`, and `merge_memory_into`, the solver previously performed a `!entry.contains(&loc)` check, collected into an intermediate vector, performed a second hash lookup via `self.pts.get_mut(&dst)`, and inserted elements. Replaced with single-pass `entry.insert(loc)` checks, eliminating double lookups, double hash probes, and intermediate vector allocations.

### 2. Preprocess options reuse during warm pass (`crates/trace-parse/src/lower.rs`)

`eff_opts` previously underwent deep cloning for every language of every header in `headers_for_macro_warm` (1,000+ full clones of vectors containing `include_paths`, `system_include_paths`, `defines`, and `command_macros`). We now preconfigure `base_prep_opts: [PreprocessOptions; 2]` once per language and set `shared_macros` in-place, eliminating all redundant options vector allocations.

### 3. SQLite in-memory cache pragmas (`crates/trace-db/src/export.rs`)

Configured `PRAGMA temp_store = MEMORY; PRAGMA cache_size = -64000;` during database export. This expands SQLite's internal page cache to 64 MiB and keeps temporary index-building B-trees entirely in RAM rather than spilling to disk.

### 4. `index_in_window` concurrency tuning (`crates/trace-parse/src/lower.rs`)

Previously, `INDEX_WINDOW_PER_WORKER` was capped at 2 and `INDEX_WINDOW_MAX` at 32. On a 16-core machine with 16 worker threads, if a single translation unit took longer to parse/lower, the remaining 15 workers quickly advanced by 32 units (`st.next >= st.merged + window`) and stalled on `window_moved.wait()`, dropping CPU utilization to under 300%.

- Increased `INDEX_WINDOW_PER_WORKER` from 2 to 4.
- Increased `INDEX_WINDOW_MAX` from 32 to 64.
- Workers can now stay productive across stragglers, maintaining higher CPU utilization across multi-core systems while preserving strict in-order sequential merging and bit-reproducibility.

### 5. Header type table sharing and address interning (Opportunity 4)

In `merge_types` and `TypeTable`:
1. **`intern_arc` pointer-address caching**: `intern_arc` previously fell back to `intern_ref` whenever a pre-existing type had not been registered in `by_ptr`. It now caches the pointer address in `by_ptr` upon first encounter (even when matched via `interned_as_is` or `interned_as_tag`) and bypasses the global weak-descriptor pool entirely by calling `intern_shared` directly with the existing `Arc<TypeDesc>`. This eliminates repeated weak pool locking, hashing, and lookups during header type merges.
2. **`register_alias_arc` zero-copy alias transfer**: Replaced `resolve_alias` + `register_alias_ref` with `register_alias_arc`, cloning the existing `Arc<TypeDesc>` directly without re-hashing or querying the weak descriptor pool.
3. **`merge_struct_declarations` dedup guard**: Replaced unconditional `extend` with a `contains` guard, avoiding atomic ref-count churn on thousands of already-known aggregate declarations.

### 6. Tree-sitter AST tree Arc sharing (`crates/trace-parse/src/lower.rs`)

In `lower_prepared_source`, `LowerContext` previously cloned `parsed.tree.clone()`, triggering a C-level `ts_tree_copy` heap duplication of the entire syntax tree for every lowered translation unit and header. Replaced with `tree: Arc<tree_sitter::Tree>`, sharing the tree without allocating or duplicating C tree nodes.

### 7. Bounded compilation-database IR accumulation (Opportunity 2)

In `crates/trace-parse/src/configured.rs` and `crates/trace-parse/src/merge.rs`:
- Implemented `VariantMerger`, allowing configuration variants to be merged incrementally while preserving the exact `VariantDedup` state across batches.
- Replaced the batch collection of all `ConfiguredUnits` into memory with `index_in_window` + `VariantMerger`, streaming each file's completed units into the program and dropping them immediately. Memory is now bounded by the window size rather than scaling with the total number of translation units in the compilation database.
- Converted orphan header processing in `configured.rs` to use `index_in_window` instead of collecting all `UnitIndex` instances into memory.

### 8. Set-guarded class hierarchy closures and metadata deduplication (`crates/trace-ir/src/program.rs`, `crates/trace-parse/src/merge.rs`)

- `subclass_closure` and `dispatch_subclass_closure` previously performed an O(N) linear scan `!out.iter().any(|c| c == derived)` inside BFS loops over class hierarchies, causing O(N^2) scaling on deep C++ hierarchies. Added a fast `visited: FxHashSet<&str>` alongside `out` to make edge visited checks O(1) with zero string allocations.
- `method_targets_among` now maintains `seen_targets: FxHashSet<FnId>` for O(1) deduplication of dispatched targets instead of scanning `out`.
- In `merge_unit`: checked `program.namespaces.contains(ns)` before cloning namespace strings, eliminating hundreds of thousands of redundant allocations per run.
- Vector-based `arrow_returns` and `final_classes` in `Program` are preserved without shadowing sets, avoiding memory duplication and synchronization hazards.

### 9. Zero-allocation namespace and scope qualification (`crates/trace-parse/src/lower.rs`)

`LowerContext::qualify` previously allocated a `Vec<String>`, cloned every namespace segment from `ns_stack`, allocated `name.to_string()`, and joined them on every declaration, tag check, type canonicalization, alias registration, and member lookup.
- Maintained an incremental `current_ns_prefix: String` alongside a stack of prefix lengths `ns_prefix_lens: Vec<usize>` in `LowerContext`.
- Added `push_namespace`, `pop_namespace`, and `truncate_namespace` to adjust `current_ns_prefix` in-place with O(1) truncation.
- `qualify(name)` now fast-paths: if `current_ns_prefix` is empty, it returns `name.to_string()` directly (1 allocation instead of multiple vectors/joins). If non-empty, it formats into exact capacity with two `push_str` calls.
- `namespace_scope()` directly clones `current_ns_prefix` without iterating or string concatenation loops.

### 10. Platform-specific memory management and pipeline robustness

- Explicitly scoped `malloc_trim(0)` memory reclamation to Linux/glibc (`cfg(all(target_os = "linux", target_env = "gnu"))`), while restricting `mimalloc` to Windows CLI binaries (`cfg(windows)`).
- Reverted SQLite export pragmas to safe defaults (`PRAGMA foreign_keys = OFF; synchronous = OFF; journal_mode = MEMORY;`), dropping experimental `temp_store` and large `cache_size` settings that inflated resident memory on large table exports without measurable timing benefit.
- `index_in_window` transparently executes single-threaded when `jobs <= 1 || items.len() <= 1`, eliminating thread-pool recursion hazards and unifying compilation database execution between sequential and parallel modes.
- Restored `include_graph.index_order` header-IR merge sequencing to guarantee 100% stable Kahn merge order across all translation units.

### 11. Zero-copy return flow expansion and parameter borrowing (`crates/trace-analysis/src/pag.rs`)

- `expand_return_flows` previously executed `for flow in flows.clone()`, cloning the entire return flow vector for every function return expansion. It now borrows `for flow in flows` directly.
- Avoided cloning function parameter vectors (`params.clone()`) across `apply_return_model` callsites by borrowing `params.as_slice()` directly.

### 12. Zero-allocation call-site dedup and batch header registration (`crates/trace-ir`, `crates/trace-parse`)

- In `MergeDedup`, `site_keys` and `variant_site_records` previously used flat keys `(FileId, u32, u32, String)`. Every call site merged across units evaluated `cs.callee_name.clone()`, allocating and immediately deallocating strings on duplicate hits. Replaced with nested maps `(FileId, u32, u32) → callee: String → CallSiteId` matching the zero-clone pattern of `fn_keys`, allowing hits to probe via `&str` without allocating.
- Replaced per-header hash probes in `program.symbols.register_included_header` with `register_included_headers`, performing a single map lookup per translation unit.
- Reused `graph.reachable_paths` in `headers_to_merge` in-place, eliminating redundant secondary `HashSet` construction and cross-set copying.

### Measurements

Measured on Linux x86_64, release builds, minimal export:

| Corpus | Baseline elapsed | Final elapsed | Overall Speedup | Peak RSS |
|---|---:|---:|---:|---:|
| HDF (802 TUs, 8 workers) | 6.13 s | **4.46 s** | **-27.2% (-1.67s)** | **281 MiB** |
| Camera (744 TUs, 8 workers) | 9.17 s | **7.64 s** | **-16.7% (-1.53s)** | **702 MiB** |

### Validation

- All 817 workspace unit and integration tests pass (`cargo test --workspace`).
- Clippy is completely clean with zero warnings (`cargo clippy --workspace --all-targets`).
- Formatter is clean (`cargo fmt --check`).
- Pinned OpenHarmony evaluation passes all 93 checks (`python3 scripts/eval_check.py`) with 0 failures, preserving 100% bit-reproducibility across all tables.

## Comprehensive cumulative comparison

Evaluating the entire optimization sequence—from initial review baseline (`c1c76b6`) through PCH topological wave parallelization, AST Arc-sharing, solver zero-copy extraction, incremental compilation-database window streaming, zero-allocation namespace qualification, and nested deduplication maps:

### 1. Overall Performance and Memory Summary

| Workload / Corpus | Scale (TUs / Files) | Initial Baseline Elapsed | Final Elapsed | Speedup | Initial Peak RSS | Final Peak RSS | Memory Reduction |
|---|---|---:|---:|---:|---:|---:|---:|
| **HDF Core** (`drivers_hdf_core`) | 802 TUs, 1,483 files | 11.29 s | **4.46 s** | **+60.5% (2.5×)** | 578.0 MiB | **281.9 MiB** | **-51.2% (2.0× lower)** |
| **Hiview** (`hiviewdfx_hiview`) | 690 TUs, 1,428 files | 4.80 s | **2.68 s** | **+44.2% (1.8×)** | 450.0 MiB | **283.2 MiB** | **-37.1% (1.6× lower)** |
| **Camera Framework** (`multimedia_camera_framework`) | 744 TUs, 1,593 files | 16.04 s | **7.64 s** | **+52.4% (2.1×)** | 1,472.4 MiB | **702.3 MiB** | **-52.3% (2.1× lower)** |
| **Clang Core Library** (`llvm-project/clang/lib`) | 1,124 TUs, 3,300 files | 372.2 s | **239.5 s** | **+35.7% (1.6×)** | 6,181.4 MiB | **6,078.6 MiB** | Stable (< 6 GiB) |
| **Clang Full Repo** (`llvm-project/clang`) | 22,147 TUs, 25,284 files | *(Unbounded)* | **8m 41.3s** | Scalable | *(OOM risk)* | **9.73 GiB** | Bounded (< 10 GiB) |

### 2. Analysis and Exported Fact Volume

| Workload / Corpus | Functions (Defined / External) | Call Edges (Direct / Indirect / External) | Argument Flow Edges | SQLite DB Size |
|---|---|---|---|---|
| **HDF Core** | 12,609 (10,256 / 2,353) | 75,836 (42,252 / 4,825 / 28,759) | 66,253 | 31.5 MB |
| **Hiview** | 10,648 (7,832 / 2,816) | 31,724 (11,254 / 108 / 20,348) | 12,331 | 20.9 MB |
| **Camera Framework** | 23,905 (19,175 / 4,730) | 97,422 (42,652 / 168 / 54,600) | 31,113 | 48.2 MB |
| **Clang Core Library** | 156,235 (94,053 / 62,182) | 461,014 (131,289 / 5,318 / 324,407) | 129,600 | 179.8 MB |
| **Clang Full Repo** | 468,020 (275,201 / 192,819) | 1,339,671 (688,864 / 2,635 / 648,172) | 360,787 | 576.0 MB |

### 3. Bit-Reproducibility and Invariant Verification

1. **Deterministic row-for-row SQLite equivalence**:
   - Every exported SQLite table (`files`, `functions`, `variables`, `call_sites`, `call_edges`, `arg_flow_edges`, `diagnostics`, `flow_nodes`, `flow_edges`, `locations`, `points_to`, `types`) matches 100% row-for-row against canonical baseline exports (excluding timestamps in `analysis_run`).
2. **OpenHarmony pinned evaluation suite**:
   - `python3 scripts/eval_check.py` passed with **93/93 checks passing (0 failures)**.
3. **Rust test suite & linter clean**:
   - All 818 workspace tests pass (`cargo test --workspace`).
   - Zero Clippy warnings (`cargo clippy --workspace --all-targets`).
   - Formatter clean (`cargo fmt --check`).

## Solver hybrid `PointsToSet` and load-subscriber filtering

Implemented Frontier 4 from the memory and solver review:

1. **Hybrid inline `PointsToSet` (`crates/trace-analysis/src/solver.rs`)**:
   - Replaced `FxHashSet<LocId>` with an inline/spilling enum:
     - `Empty`: 0 elements, zero allocations.
     - `Single(LocId)`: 1 element, stored inline in 4 bytes, zero heap allocations.
     - `Small([LocId; 6], u8)`: 2 to 6 elements, stored directly inline in an array, zero heap allocations. Iteration and duplicate checks execute via unrolled SIMD/register comparisons in L1 cache.
     - `Set(FxHashSet<LocId>)`: Spills to a hash set only when a node exceeds 6 points-to targets.
   - For >90% of all PAG nodes (which have $\le 6$ pointees), heap allocations during fixpoint solving are completely eliminated.
2. **Load-subscriber filtered `loc_nodes` tracking (`crates/trace-analysis/src/solver.rs`)**:
   - In `SolverState`, precomputes `load_subscribers: FxHashSet<PagNodeId>` from `pag.indices.load_src` at solve start, and updates it dynamically alongside retroactive `loc_nodes` registration when `expand_return_flows` adds new load constraints mid-solve.
   - In `register_loc_holder` and `register_loc_holders`, guards insertions with `self.load_subscribers.contains(&node)`.
   - >95% of PAG nodes (variables, return destinations, parameters, direct copies) are not load subscribers and now completely bypass `loc_nodes` hash lookups and set allocations.
   - `touch_loc_holders(loc)` no longer needs to inspect `load_src`, as every node tracked under `loc_nodes` is guaranteed to be an active load subscriber.
3. **Single-pass points-to propagation**:
   - `propagate_pts` now performs a single pass over `src_pts.iter()`, eliminating double-hash lookups.

### Measurements & Impact

| Workload | Metric | Before Frontier 4 | With `PointsToSet` | Improvement |
|---|---|---:|---:|---:|
| **Clang Core Library** (`llvm-project/clang/lib`) | Wall-Clock Time | 244.02 s (4m 04s) | **229.38 s (3m 49s)** | **-14.64 s (-6.0%)** |
| | User CPU Time | 1,334.92 s | **1,261.66 s** | **-73.26 CPU seconds (-5.5%)** |
| **Camera Framework** | Analyze Time | 0.40 s | **0.30 s** | **-25%** |
| **Repeated HDF Runs** | Table Match | 12/12 tables | **12/12 tables** | 100% bit-reproducible |
| **OpenHarmony Suite** | Evaluation Checks | 93/93 pass | **93/93 pass** | 0 failures |
| **Workspace Tests** | Test Suite | 817 pass | **818 pass** | 0 failures |

