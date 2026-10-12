# Profile-guided release build experiment

`scripts/build_pgo.py` builds an ordinary ThinLTO CLI, builds and trains an
instrumented CLI, merges its LLVM profiles, then rebuilds with those profiles.
It measures the ordinary and PGO binaries in fresh processes with identical
analysis options. `--fat-lto` adds fat LTO with and without PGO, using its own
instrumentation/training pass. The workspace release profile and release CI
remain the normal build path.

The workflow supports native Linux and macOS.
It requires Python 3.10+, or Python 3.11+ when Cargo configuration files are
present (for the standard-library TOML reader).

Use the stable toolchain that CI builds with, for example
`RUSTUP_TOOLCHAIN=stable`, and install its matching profiling tools. The full
[Linux experiment](EVAL_REPORT.md#native-lto-and-pgo-build-experiment--2026-10-12)
was checked with stable Rust **1.95.0 (LLVM 22.1.2)**. A reviewer also checked
instrumented hiview training on an Apple M1 with stable **1.98 (LLVM 22)**:
it trained successfully and wrote a `.profraw`, but the full macOS comparison
was not completed.

Instrumented builds are toolchain-sensitive on Apple Silicon. In that review,
nightly **1.100.0 (2026-08-26, LLVM 23)** crashed in LLVM's profiler runtime
during training, including with `--jobs 1`. For a failure reported as
`analysis failed (-11)` (signal 11), check the crash report for
`__llvm_profile_instrument_target` and retry with stable and matching
`llvm-profdata`.

Install the selected toolchain's profiling tools and verify the pinned corpora
before starting:

```bash
export RUSTUP_TOOLCHAIN=stable
rustup component add llvm-tools-preview
python3 scripts/fetch_corpora.py
python3 scripts/build_pgo.py --runs 5
```

The default training projects are HDF, hiview and camera, at the revisions in
`scripts/eval_expected.json`. `--corpus-base DIR` or `TRACE_CORPUS_BASE` changes
their checkout location. A missing, dirty, or incorrectly pinned training tree
fails preflight. Nothing fetches or changes checkouts automatically.

Validate generalization using clean Git checkouts or subtrees excluded from
training; for example, Ability runtime and a manageable LLVM/Clang test subtree:

```bash
python3 scripts/build_pgo.py --runs 5 --fat-lto \
  --holdout "~/ability_ability_runtime" \
  --holdout "~/llvm-project/clang/test/Parser"
```

Holdouts never enter the training profile. Their paths must not overlap training
trees. Their Git revisions are recorded, and local changes inside the selected
tree are rejected, with untracked-file checks forced regardless of Git
configuration. Pin these revisions when comparing separate experiments.
Git-ignored files inside every training and holdout tree are also recorded by
file count and content digest, including generated sources and compilation
databases. All ignored files are hashed to cover analysis inputs regardless of
extension. Tracked file symlinks also contribute their link spelling, resolved
target path and target contents, including targets outside the checkout.
Changing either snapshot invalidates the experiment at the final check.
Tracked submodules, ignored nested repositories and directory symlinks are
rejected because their contents cannot be verified by this snapshot. Select a
submodule as its own corpus, or select a subtree that excludes it.
For projects already pinned in the eval manifest, reserve a corpus by leaving it
out of `--train` and naming its checkout as `--holdout`.
CLI file and directory paths expand `~`, including when quoted, and resolve
relative paths and symlinks before validation.

Additional controls:

| Option | Behavior |
|---|---|
| `--jobs 8` | Indexing workers, identical for training and measurements |
| `--build-jobs 2` | Cargo compilation parallelism |
| `--features mimalloc` | Use the same allocator feature in every build |
| `--training-modes minimal full explore` | Train and compare these modes on every project; exploration uses budget 4 |
| `--budget N` | Fixed solver pop budget; default is the pinned eval budget, 0 is unlimited |
| `--timeout 1800` | Seconds allowed per analysis; stops the wrapper and analyzer on timeout |
| `--outdir PATH` | A new artifact directory disjoint from corpus trees; existing directories are rejected |
| `--llvm-profdata PATH` | Override the tool from rustc's sysroot; LLVM major must match rustc |

Compilation databases are automatically discovered by `trace analyze` as in
normal usage. Every workflow analysis uses `--require-local-inputs`: a header or
build metadata file consumed from outside the selected corpus fails the run,
including nested, macro-only and forced headers, recursive link response files,
compilation/link databases, CMake JSON references and symlink targets outside
the tree. Keep these dependencies inside the verified corpus tree. This also
applies to configured workloads with compilation databases or shared flags;
unused external search paths alone do not fail. Inspection, LSP and C API usage are not trained by this CLI
experiment.

## Artifacts and correctness

Each invocation creates a fresh `target/pgo/run-*` directory unless `--outdir`
is supplied. Both explicit output paths and the default `target/pgo` parent
must neither contain a corpus nor be inside one, including through symlinks.
Separate Cargo target directories isolate baseline, instrumented,
and optimized builds. The final binary is named in `report.json`, for example
`thin-pgo/<host-triple>/release/trace`; ordinary `target/release/trace` is not
overwritten. Scratch databases use `TemporaryDirectory` and are removed on
success or failure. Build, training, and benchmark logs and profiles remain in
the artifact directory, including on failure.

`report.json` records source revision/status/content digest, Rust/Cargo/LLVM
versions, selected corpus revisions and ignored-file/symlink digests, flags/features,
binary/profile hashes, binary sizes, build times, environment overrides, per-run measurements and
median/minimum/maximum summaries. A changed source checkout or corpus revision
invalidates the experiment; the artifact directory itself is excluded from the
source snapshot. New invocations never reuse old profiles.

Every warmup and measured run must match the baseline's sorted contents and
schema of **every SQLite table**, preserving duplicate rows and IDs. Only
`analysis_run.created_at` and `analysis_run.trace_version` are excluded. Solver
options, convergence status and pop counts remain checked. A mismatch fails the
experiment; there is no tolerance band. The previous scratch database is removed
before every invocation; a successful exit without a new database fails instead
of checking stale output. Build/analysis errors, missing profiles,
missing measurements and timeouts also fail and are recorded in the report.

Before adopting a candidate, also run workspace tests and the existing pinned
corpus correctness probes with its binary:

```bash
cargo test --workspace
python3 scripts/test_build_pgo.py
python3 scripts/eval_check.py --bin /absolute/path/to/thin-pgo/HOST/release/trace
```

## Reading the measurements

All candidate builds finish before timing begins. Each case gets an untimed
warmup for each binary. Timed processes rotate the binary order to reduce order
bias; they use warm filesystem caches without a persistent IR cache. SQLite
comparison runs outside measured process time. Wall time comes from
`/usr/bin/time`, with 0.01-second resolution, and includes analyzer process
startup. It excludes Python's timeout polling delay. Index, analysis and export times come from
the CLI and are rounded to **0.1 seconds**; use whole-process timings to assess
small improvements. Peak RSS is recorded in bytes; on macOS this is RSS rather
than the physical-footprint measurement in the separate memory observer.
Child processes run with `LC_ALL=C`, recorded in the report, to keep timer
formatting reproducible. The parser also accepts comma-decimal timer logs.
Logs are decoded as UTF-8 with replacement for invalid bytes.

`speedup_vs_thin` is baseline median wall time divided by candidate median wall
time: values above 1 are faster. Geometric means are reported separately for
training and holdout cases, with equal weight per case. Inspect each project's
spread and regressions before accepting an aggregate improvement. Compilation
times are fresh builds including dependencies, not incremental build costs.

Training executes each selected project/mode once. Large projects contribute
more execution counts naturally; this initial version does not normalize or
manually weight profile inputs. The report records this policy. Training only on
large projects can underrepresent other workflows; select representative cases
and retain holdouts. Instrumented execution times are not performance results.

PGO uses `RUSTFLAGS` across runtime Rust crates and an explicit native `--target`
to avoid instrumenting build scripts and proc macros. Thin/fat LTO and one
codegen unit are set consistently in each pair. Every build appends
`-Ccodegen-units=1` to override named-package and dependency-wildcard Cargo
profile settings, as well as setting the global release profile. Other inherited build settings
remain consistent and are recorded. Inherited PGO/LTO/codegen-unit Rust flags
are rejected to avoid conflicting settings, including hyphen and underscore
spellings in `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS`. Missing-function profile
warnings are enabled and retained in optimized build logs; unexecuted functions can
legitimately have no profile.

Pass the experiment's effective Rust flags explicitly through `RUSTFLAGS` or
`CARGO_ENCODED_RUSTFLAGS`. If neither variable is supplied, preflight rejects
`build.rustflags`, target-specific Rust flags, and their `CARGO_BUILD_RUSTFLAGS`
and `CARGO_TARGET_*_RUSTFLAGS` environment aliases. This prevents the workflow
from silently overriding required cfg, CPU or linker flags. Explicit flag
variables use [Cargo's precedence](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags);
use the encoded form for individual arguments containing whitespace. The script
does not reproduce Cargo's target/cfg flag merging. Config-file `include`
directives are also rejected.

Compiler selection honors `RUSTC`, `CARGO_BUILD_RUSTC`, and `build.rustc` in
Cargo's workspace/ancestor/home configuration hierarchy, including config-relative
executable paths and the legacy `.cargo/config` filename. The selected compiler
is passed explicitly through `RUSTC` to all Cargo builds and used for version,
host and sysroot checks. `report.json` records the selection and configuration
file hashes; changes to those settings invalidate the final check.

Native tree-sitter, SQLite and allocator C code are built normally: Rust flags
do not instrument those libraries or enable cross-language LTO. Likewise this
experiment does not rebuild the Rust standard library. Separate native compiler
configuration would be needed to extend coverage. Target-specific release
training, including musl/Windows and cross-compiled ARM binaries, needs a runner
that can execute and validate the shipped target; native-host results alone do
not establish their benefit.

Rust's workflow is documented in the
[rustc PGO guide](https://doc.rust-lang.org/rustc/profile-guided-optimization.html);
LTO settings are described in the
[Cargo profile reference](https://doc.rust-lang.org/cargo/reference/profiles.html#lto).
Record actual experiment results in [EVAL_REPORT.md](EVAL_REPORT.md), and only
add release CI training after repeated measurements justify its cost.
