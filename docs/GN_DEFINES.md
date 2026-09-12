# GN define evidence

The `conditional_coverage` example emits ranked configuration candidates for
issue #58 alongside the conditional-region records from #57. These are evidence
from the checkout, **not applied configuration or per-TU accuracy**.

```bash
cargo run -p trace-cli --release --example conditional_coverage -- /path/to/project > coverage.tsv
```

Only `BUILD.gn`, `*.gni` and `*.gn` are parsed. Direct string entries in `defines = [...]`
and `defines += [...]` are recorded. Each occurrence is retained, including
conflicting values and names no C/C++ condition reads. Comments, unrelated
strings, member assignments, removals (`-=`), computed list entries, and list
arithmetic are excluded; an expression such as `defines = ["A"] + ["B"]` is
skipped whole, so neither list is recorded. Names must be C identifiers. A
dynamic name such as `"${prefix}_FEATURE"` is skipped; `"FEATURE=$value"`
preserves `$value` as an unevaluated expression with low confidence. GN quote,
dollar and backslash escapes follow the [GN language reference](https://gn.googlesource.com/gn/+/main/docs/language.md).

The tab-separated record has these fields:

```text
GN_DEFINE  name  has-value  value  file  line  confidence  conditions
```

`has-value` is `0` for `"NAME"` and `1` for `"NAME=value"`, including `"NAME="`.
Paths are relative to the scanned root; lines identify the opening quote of the
entry. Nested conditions are joined with `&&`, preserving expression text.
An `else` includes the negation of earlier branch conditions. Neither the
conditions nor interpolated values are evaluated. Tabs and newlines in fields
are replaced by spaces, as in the other coverage records.

| Confidence | Evidence |
|------------|----------|
| high | Unconditional literal in a recognized target block: `executable`, `shared_library`, `static_library`, `source_set`, `loadable_module`, or the `ohos_` forms of the first four. |
| medium | One enclosing condition, or other scopes such as a config, file scope, or an unrecognized custom target. |
| low | Multiple enclosing conditions (including else-if alternatives), a template or loop, or an interpolated value. |

The report sorts by confidence, then by always-excluded lines in chains that
read the name, then by name and location. It shows the first 40 entries and the
total count; the TSV keeps every entry. Shared lines overlap across names and do
not predict how many lines a define would recover. A candidate whose name no
conditional chain reads has no line evidence at all and is shown as `—`, as
against `0 / 0` for a name that is read but gates no always-excluded lines.
Existing build-file mentions in `NAME` records remain separate evidence;
mentioning a name is not proof that the build defines it.

This scanner does not execute GN, resolve imports, instantiate templates,
propagate configs, map targets to sources, or reconcile later overwrites and
removals. Files are scanned independently, so even high confidence means only
stronger syntactic evidence. Unreadable build files are skipped, matching the
existing mention scan; malformed or unsupported constructs may be omitted.
CMake, Make and Kconfig inference remain deferred.

Regenerate the corpus report using the recipe in
[CONDITIONAL_COVERAGE.md](CONDITIONAL_COVERAGE.md), which invokes
`scripts/gen_conditional_coverage_report.py`. Existing TSV captures without
`GN_DEFINE` rows remain readable but need recapturing to include this evidence.
