# Contributing to trace

Thanks for your interest in contributing to **trace**.

## Development prerequisites

- **Rust** (stable toolchain, edition 2021)
- **Cargo** (ships with rustup)
- **tree-sitter CLI** (optional — only needed to regenerate grammars)

Clone and build:

```bash
git clone <repo-url> && cd pathologist
cargo build --workspace
```

## Workspace commands

| Action | Command |
|--------|---------|
| Build all crates | `cargo build --workspace` |
| Build release binary | `cargo build --release` |
| Run full test suite | `cargo test --workspace` |
| Run a single crate's tests | `cargo test -p trace-analysis` |
| Run a specific test | `cargo test -p trace-analysis -- test_name` |
| Run the CLI (release) | `cargo run -p trace-cli --release -- analyze <TARGET> -o /tmp/out.db` |
| Run integration fixture | `cargo run -p trace-cli --release -- analyze tests/fixtures/<name> -o /tmp/<name>.db` |

Always rebuild with `--release` when benchmarking or profiling; the debug
binary is significantly slower.

## Test projects (fixtures and eval corpora)

`tests/fixtures/` contains small C/C++ corpora used by integration tests. Each
fixture is a directory with `.c` / `.cpp` sources and an optional
`expected.json`:

```text
tests/fixtures/
  direct_call/                 Direct call-by-name resolution
  indirect_call/               Fn-ptr / indirect call resolution
  hpp_designated_dispatch/     C++ fn-ptr indirect dispatch
  arg_flow/                    Argument flow from actuals to formals
  preproc/                     Preprocessor directive fixtures
  builtin_macros/              Builtin fallback macros (HWTEST, gMock, kernel)
  ...
```

Beyond the fixtures, `scripts/` runs evaluation against large third-party
trees (e.g. OpenHarmony subprojects). These corpora are pinned by revision
in `scripts/eval_expected.json` and are **not** checked into the repo:

```bash
python3 scripts/fetch_corpora.py   # download once
python3 scripts/eval_check.py      # verify counts; re-capture when output legitimately changes
```

Re-capture the expectations in `eval_expected.json` only when a change
legitimately moves the counts. Do not pin env-specific or machine-dependent
values.

## Formatting and style

- Run `cargo fmt --workspace` before committing.
- Run `cargo clippy --workspace` and address all warnings.
- Follow existing code conventions — see `AGENTS.md` for crate-level guidance.

## Adding fixtures and integration tests

1. Create a directory under `tests/fixtures/<fixture_name>/`.
2. Add one or more `.c` / `.cpp` source files that exercise the feature.
3. Optionally add `expected.json` capturing expected metric counts; run
   `python3 scripts/eval_check.py` to verify.
4. Add a Rust integration test (typically in the relevant crate's `tests/` or
   in `crates/trace-cli/tests/`) that invokes the pipeline on the fixture and
   asserts on exported data.

## Adding new analysis constraints

Document the new constraint kind in `docs/ANALYSIS.md` **before** adding code.
Then follow the steps in `AGENTS.md` ("Adding analysis constraints" section).

## Schema and preprocessor changes

- **SQLite schema** — update `trace-db/src/schema.rs`, `trace-db/src/export.rs`,
  and `docs/SQLITE_SCHEMA.md`. Document the schema version bump in `README.md`.
- **Preprocessor** — document the new directive or feature in
  `docs/PREPROCESSOR.md`, add lexer tests, and add a fixture under
  `tests/fixtures/preproc/` — or under `tests/fixtures/builtin_macros/` when
  the feature is a builtin fallback macro, whose fixtures are grouped there.
- **Flow constraints** — document the new kind in `docs/ANALYSIS.md` and
  update the phase table if applicable.

## Pull request expectations

- Keep PRs focused — one logical change per PR.
- All CI checks must pass (`cargo test --workspace`, `cargo clippy`,
  `cargo fmt --check`).
- Write a clear PR description explaining the *what* and *why*.
- Reference any related issues.
- If your change touches analysis, preprocessor, or schema, note the
  relevant documentation updates in the PR description.

## Reporting issues

Open an issue at the repository's issue tracker with:

- A minimal reproduction (`.c` file + reproduce command).
- The expected vs. actual behaviour.
- Your `trace` version (`trace --version` or Cargo.toml version).

## Code of conduct

We are committed to providing a welcoming, inclusive, and harassment-free experience for everyone. This project and everyone participating in it is governed by our [Code of Conduct](CODE_OF_CONDUCT.md). By participating, you are expected to uphold this code. Please report unacceptable behavior according to the instructions in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
