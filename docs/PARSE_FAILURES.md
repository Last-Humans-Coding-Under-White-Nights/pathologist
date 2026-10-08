# Parse-failure reports

Generate a local snapshot of parser diagnostics for the corpora pinned in
`scripts/eval_expected.json`. Captured file lists and counts depend on the build
and configuration; current regression expectations belong in that JSON file,
and dated measurements belong in [EVAL_REPORT.md](EVAL_REPORT.md).

Run from the repository root:

```bash
set -euo pipefail   # stop at the first failure, do not run on with stale inputs

# One corpus base for every step: fetch_corpora.py, the analyze runs
# below and this script all read $TRACE_CORPUS_BASE.
export TRACE_CORPUS_BASE="${TRACE_CORPUS_BASE:-$HOME}"

python3 scripts/fetch_corpora.py   # corpora at the revisions pinned in scripts/eval_expected.json
cargo build --release -p trace-cli && cargo build --release -p trace-cli --examples

# One analyze + one TSV per corpus, read back by these exact names.
# Each TSV is written to a .part file and renamed only if the
# command succeeded, so a failed run leaves no final file at all
# The generator validates completion records. Clear stale files first.
rm -f /tmp/parse_failures_{hdf,hiview,camera}.tsv{,.part}
target/release/trace analyze "$TRACE_CORPUS_BASE/drivers_hdf_core" -o /tmp/hdf_parse_check.db --jobs 8
target/release/examples/parse_failures "$TRACE_CORPUS_BASE/drivers_hdf_core" --from-db /tmp/hdf_parse_check.db > /tmp/parse_failures_hdf.tsv.part
mv /tmp/parse_failures_hdf.tsv.part /tmp/parse_failures_hdf.tsv
target/release/trace analyze "$TRACE_CORPUS_BASE/hiviewdfx_hiview" -o /tmp/hiview_parse_check.db --jobs 8
target/release/examples/parse_failures "$TRACE_CORPUS_BASE/hiviewdfx_hiview" --from-db /tmp/hiview_parse_check.db > /tmp/parse_failures_hiview.tsv.part
mv /tmp/parse_failures_hiview.tsv.part /tmp/parse_failures_hiview.tsv
target/release/trace analyze "$TRACE_CORPUS_BASE/multimedia_camera_framework" -o /tmp/camera_parse_check.db --jobs 8
target/release/examples/parse_failures "$TRACE_CORPUS_BASE/multimedia_camera_framework" --from-db /tmp/camera_parse_check.db > /tmp/parse_failures_camera.tsv.part
mv /tmp/parse_failures_camera.tsv.part /tmp/parse_failures_camera.tsv

python3 scripts/gen_parse_failures_report.py --output /tmp/parse_failures_report.md
```

The `parse_failures` example re-preprocesses with whatever build runs it; the DB only selects the failing-file set. Build the binary *and* the examples (the `--examples` flag alone leaves `target/release/trace` stale).

The generator writes `/tmp/parse_failures_report.md` by default. `--output`
selects another destination; generated snapshots are kept outside the maintained
documentation.

The TSV ends with `END<tab>row_count`. A successful capture with no reported
failures contains `END<tab>0`; an empty file is invalid. Missing, truncated, or
mismatched completion records stop report generation. Regenerate older captures.

`--from-db` selects only the paths with parse-stage failure diagnostics. A database
with no matching diagnostics selects no files. It is opened read-only; a missing
database is an error. Without `--from-db`, the example scans discovered sources
and headers. File order and category ties are deterministic.

The example re-preprocesses selected files with inferred include paths and default
options. It does not replay compilation commands, command-line defines, dependency
roots, cached header variants, or exploratory configurations from the database.
Headers reachable from C++ units use the C++ grammar, so a header consumed in
both languages is represented by one grammar. Results are diagnostic aids rather
than a reconstruction of every failed indexing context. Snippet coordinates refer
to preprocessed text, not original source positions.

Categories are heuristics based on parser nodes and snippets. A missing semicolon
alone does not establish that a test macro caused the failure. Inspect the source
and preprocessing diagnostics before assigning a cause.
