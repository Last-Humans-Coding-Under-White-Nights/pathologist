# Conditional-compilation coverage

Measure what a single default configuration excludes: every `#if` / `#ifdef` / `#ifndef` chain, which arm each preprocess run took, the source lines in the arms not taken, and what the checkout knows about the names the conditions read. Reporting only — preprocessing behaviour is unchanged. Regenerate with:

```bash
set -euo pipefail   # stop at the first failure, do not run on with stale inputs

export TRACE_CORPUS_BASE="${TRACE_CORPUS_BASE:-$HOME}"
python3 scripts/fetch_corpora.py   # corpora at the revisions pinned in scripts/eval_expected.json
cargo build --release -p trace-cli --examples

# Each TSV is written to a .part file and renamed only if the command
# succeeded; the generator treats a MISSING file as an error.
rm -f /tmp/conditional_coverage_{hdf,hiview,camera}.tsv{,.part}
target/release/examples/conditional_coverage "$TRACE_CORPUS_BASE/drivers_hdf_core" > /tmp/conditional_coverage_hdf.tsv.part
mv /tmp/conditional_coverage_hdf.tsv.part /tmp/conditional_coverage_hdf.tsv
target/release/examples/conditional_coverage "$TRACE_CORPUS_BASE/hiviewdfx_hiview" > /tmp/conditional_coverage_hiview.tsv.part
mv /tmp/conditional_coverage_hiview.tsv.part /tmp/conditional_coverage_hiview.tsv
target/release/examples/conditional_coverage "$TRACE_CORPUS_BASE/multimedia_camera_framework" > /tmp/conditional_coverage_camera.tsv.part
mv /tmp/conditional_coverage_camera.tsv.part /tmp/conditional_coverage_camera.tsv

python3 scripts/gen_conditional_coverage_report.py --output /tmp/conditional_coverage_report.md
```

## How to read this

- **The record is the chain, not the macro.** A chain is one `#if`/`#ifdef`/`#ifndef` with its `#elif`/`#else` arms. The lines an arm excludes belong to the whole expression that controls the chain; for `#if A && B` crediting them to `A` and to `B` separately double-counts and overstates what defining either one would recover. The generated per-name view therefore splits lines into *sole* (the name is the chain's only dependency) and *shared* (listed under every name of the chain).
- **Environment.** Every translation unit is preprocessed from the command-line defines alone with its includes expanded inline in this reporting example and headers no unit reaches are preprocessed standalone, as the indexer does with orphans. No expansion cache: a cache hit replays a header's text without re-evaluating its conditionals. A header reached from several units is evaluated once per unit, so an arm can be taken in some runs and not in others (*sometimes excluded*); *always excluded* arms were never taken by any run. File totals include headers resolved outside the root through `--include`. Missing or empty source trees and hard input failures stop the measurement without publishing TSV. Command-line metadata retains `-D` values. A final completion record counts all preceding TSV rows; missing or mismatched completion records are rejected, including captures cut off at a complete line. Older TSV files must be regenerated.
- **Which arm.** An undefined name does not always select `#else`: `#if !X` with `X` unknown takes the first arm. Each arm's outcome is recorded per run rather than assumed.
- **Names read** are what the evaluation consulted, macro expansion included (`#if HAS_X` with `#define HAS_X defined(X)` reads both). *Unbound reads* count the evaluations that found no macro bound to the name — the cases that resolved against the default of `0`. An arm that was never evaluated contributes only the identifiers it spells.
- **Classes.** *include-guard*: tested by a chain that wraps a whole file (`#ifndef X` first, `#define X` next, no `#else`, nothing after its `#endif`) and by no other chain — a default-value idiom alone in a file has the guard shape, and an `#if X > 1` elsewhere that depends on the name says it is configuration. *toolchain*: a macro gcc/clang predefine (a fixed list — language, compiler, target OS, architecture, type sizes, `__has_*`). *configuration*: a `-D`, an in-tree `#define` in any region (comments and string literals ignored), or a name an in-tree build file spells (GN, CMake, Make, Kconfig — spelled, not parsed). Separately, GN define candidates record direct string entries in `defines = [...]` and `defines += [...]` in `BUILD.gn`, `*.gni` and `*.gn`, with values, entry locations, conditions and confidence. Computed entries and interpolated names are skipped. *unknown*: nothing in the checkout accounts for it. Unknown names have no supporting evidence in the scanned checkout.
- **Lines are source lines** strictly between the arm's directive and the next directive of its chain, not reachable code: a nested chain's directive lines count, blank and comment lines and continuation lines of multiline conditions count, and an always-excluded outer arm hides its inner chains (they are *never evaluated* and add nothing). Sometimes-excluded lines of nested chains can overlap. Excluded lines are not an acceptance metric on their own — what matters for exploration is whether the excluded arms hold new, source-verified driver and callback targets.

The generator writes `/tmp/conditional_coverage_report.md` by default.
`--output` selects another destination. Generated tables are local snapshots;
record dated measurements in [EVAL_REPORT.md](EVAL_REPORT.md).

The example accepts `--include` and `-D`, but does not replay compilation databases
or the analyzer's `--explore` variants. Its totals describe the configuration it
preprocessed, not every configuration a project can build. GN candidate evidence
and its limits are documented in [GN_DEFINES.md](GN_DEFINES.md).
