#!/usr/bin/env python3
"""Build docs/CONDITIONAL_COVERAGE.md from conditional_coverage example TSV output (#57)."""

from __future__ import annotations

import os
import shlex
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
OUT = REPO / "docs" / "CONDITIONAL_COVERAGE.md"

# Same layout convention as eval_check.py / gen_parse_failures_report.py:
# one checkout per corpus under $TRACE_CORPUS_BASE (default ~).
CORPUS_ENV = "TRACE_CORPUS_BASE"
CORPUS_BASE = Path(os.path.expanduser(os.environ.get(CORPUS_ENV) or "~"))

CORPORA = [
    {"id": "hdf", "name": "drivers_hdf_core"},
    {"id": "hiview", "name": "hiviewdfx_hiview"},
    {"id": "camera", "name": "multimedia_camera_framework"},
]
for _c in CORPORA:
    _c["root"] = CORPUS_BASE / _c["name"]
    _c["tsv"] = Path(f"/tmp/conditional_coverage_{_c['id']}.tsv")

# Record kinds examples/conditional_coverage.rs emits, with their field
# counts (the record kind included). Anything else means the TSV did not
# come from that tool, or came from a different version of it.
TSV_FIELDS = {"META": 3, "FILE": 5, "CHAIN": 7, "ARM": 12, "READ": 8, "NAME": 7,
              "GN_DEFINE": 8}
CLASSES = ["configuration", "unknown", "toolchain", "include-guard"]
# Confidence labels examples/support/gn_defines.rs emits, worst-last.
RANKS = {"high": 0, "medium": 1, "low": 2}
TOP_N = 40


@dataclass
class Arm:
    directive: str
    line: int
    end_line: int
    taken: int
    skipped: int
    unevaluated: int
    evaluated: int
    expression: str
    reads: dict[str, tuple[int, int, int]] = field(default_factory=dict)

    @property
    def body_lines(self) -> int:
        return max(self.end_line - self.line - 1, 0)

    @property
    def always_excluded(self) -> bool:
        return self.taken == 0 and self.skipped > 0

    @property
    def sometimes_excluded(self) -> bool:
        return self.taken > 0 and self.skipped > 0


@dataclass
class Chain:
    path: str
    line: int
    depth: int
    guard: bool
    terminated: bool
    runs: int
    arms: list[Arm] = field(default_factory=list)

    @property
    def key(self) -> str:
        return " / ".join(
            f"#{a.directive} {a.expression}".rstrip() for a in self.arms
        )

    @property
    def names(self) -> list[str]:
        seen: list[str] = []
        for a in self.arms:
            for n in a.reads:
                if n not in seen:
                    seen.append(n)
        return seen

    @property
    def evaluated(self) -> bool:
        """Met in an active context by at least one run."""
        return bool(self.arms) and (self.arms[0].taken + self.arms[0].skipped) > 0

    @property
    def always_excluded_lines(self) -> int:
        return sum(a.body_lines for a in self.arms if a.always_excluded)

    @property
    def sometimes_excluded_lines(self) -> int:
        return sum(a.body_lines for a in self.arms if a.sometimes_excluded)


@dataclass
class NameInfo:
    cls: str
    cli: bool
    defines: int
    define_site: str
    build_file: str


@dataclass
class GnCandidate:
    name: str
    value: str | None
    path: str
    line: int
    confidence: str
    conditions: str


@dataclass
class Corpus:
    files: dict[str, tuple[str, int, int]] = field(default_factory=dict)  # path -> kind, lines, runs
    chains: dict[tuple[str, int], Chain] = field(default_factory=dict)
    names: dict[str, NameInfo] = field(default_factory=dict)
    defines: str = ""
    gn_candidates: list[GnCandidate] = field(default_factory=list)


def die(msg: str) -> None:
    sys.exit(f"{msg} -- regenerate it (see the recipe at the top of "
             f"{OUT.relative_to(REPO)}); refusing to write a partial report")


def load_tsv(tsv: Path) -> Corpus:
    # Absence is the failure signal (the recipe writes `.part` and renames on
    # success); every row is validated structurally so a truncated or foreign
    # file cannot pass as a small corpus.
    if not tsv.exists():
        die(f"{tsv} does not exist")
    with tsv.open(encoding="utf-8", newline="") as source:
        text = source.read()
    if not text or not text.endswith("\n"):
        die(f"{tsv}: empty or does not end in a newline, so it was truncated")
    rows = text[:-1].split("\n")
    footer = rows.pop().split("\t")
    if len(footer) != 2 or footer[0] != "END" or footer[1] != str(len(rows)):
        die(f"{tsv}: missing or mismatched completion record")
    corpus = Corpus()
    # Records are newline-separated, exactly as the writer emits them: the
    # writer escapes only `\t` and `\n` in its fields, so a form feed or a
    # lone CR in an expression is data, and `str.splitlines()` (which breaks
    # on those too) would cut the row in half.
    for lineno, line in enumerate(rows, 1):
        parts = line.split("\t")
        kind = parts[0]
        if kind not in TSV_FIELDS or len(parts) != TSV_FIELDS[kind]:
            die(f"{tsv}:{lineno}: malformed row: {line!r}")
        try:
            if kind == "META":
                if parts[1] == "defines":
                    corpus.defines = parts[2]
            elif kind == "FILE":
                _, path, fkind, lines, runs = parts
                corpus.files[path] = (fkind, int(lines), int(runs))
            elif kind == "CHAIN":
                _, path, cline, depth, guard, term, runs = parts
                corpus.chains[(path, int(cline))] = Chain(
                    path, int(cline), int(depth), guard == "1", term == "1", int(runs)
                )
            elif kind == "ARM":
                (_, path, cline, idx, directive, aline, end, taken, skipped,
                 unev, evaluated, expr) = parts
                chain = corpus.chains[(path, int(cline))]
                if int(idx) != len(chain.arms):
                    die(f"{tsv}:{lineno}: ARM index out of order")
                chain.arms.append(Arm(directive, int(aline), int(end), int(taken),
                                      int(skipped), int(unev), int(evaluated), expr))
            elif kind == "READ":
                _, path, cline, idx, name, bound, unbound, unknown = parts
                arm = corpus.chains[(path, int(cline))].arms[int(idx)]
                arm.reads[name] = (int(bound), int(unbound), int(unknown))
            elif kind == "NAME":
                _, name, cls, cli, defines, site, build = parts
                if cls not in CLASSES:
                    die(f"{tsv}:{lineno}: unknown class {cls!r}")
                corpus.names[name] = NameInfo(cls, cli == "1", int(defines), site, build)
            elif kind == "GN_DEFINE":
                _, name, has_value, value, path, entry_line, confidence, conditions = parts
                if has_value not in ("0", "1"):
                    die(f"{tsv}:{lineno}: has-value is {has_value!r}, not 0 or 1")
                if confidence not in RANKS:
                    die(f"{tsv}:{lineno}: unknown confidence {confidence!r}")
                if int(entry_line) < 1:
                    die(f"{tsv}:{lineno}: entry line {entry_line!r} is not 1-based")
                if has_value == "0" and value:
                    die(f"{tsv}:{lineno}: value {value!r} on a candidate with no value")
                corpus.gn_candidates.append(GnCandidate(
                    name, value if has_value == "1" else None, path, int(entry_line),
                    confidence, conditions))
        except (KeyError, IndexError, ValueError) as e:
            die(f"{tsv}:{lineno}: {e.__class__.__name__} on row {line!r}")
    return corpus


@dataclass
class NameStats:
    chains: int = 0
    sole: int = 0
    shared: int = 0
    sometimes: int = 0
    bound: int = 0
    unbound: int = 0
    unknown: int = 0


NO_STATS = NameStats()


def name_stats(corpus: Corpus) -> dict[str, NameStats]:
    stats: dict[str, NameStats] = defaultdict(NameStats)
    for chain in corpus.chains.values():
        names = chain.names
        always = chain.always_excluded_lines
        sometimes = chain.sometimes_excluded_lines
        for n in names:
            s = stats[n]
            s.chains += 1
            if len(names) == 1:
                s.sole += always
            else:
                s.shared += always
            s.sometimes += sometimes
        for arm in chain.arms:
            for n, (b, u, k) in arm.reads.items():
                stats[n].bound += b
                stats[n].unbound += u
                stats[n].unknown += k
    return stats


def cls_of(corpus: Corpus, name: str) -> str:
    info = corpus.names.get(name)
    return info.cls if info else "unknown"


def fmt(n: int) -> str:
    return f"{n:,}"


def pct(part: int, whole: int) -> str:
    return "—" if whole == 0 else f"{100.0 * part / whole:.1f}%"


def code(s: str) -> str:
    return "`" + s.replace("|", "\\|").replace("`", "'") + "`"


def summarize(corpus: Corpus) -> dict:
    total_lines = sum(l for _, l, _ in corpus.files.values())
    tus = sum(1 for k, _, _ in corpus.files.values() if k == "tu")
    headers = len(corpus.files) - tus
    chains = list(corpus.chains.values())
    evaluated = [c for c in chains if c.evaluated]
    never = [c for c in chains if not c.evaluated]
    guards = [c for c in chains if c.guard]
    always = sum(c.always_excluded_lines for c in evaluated)
    sometimes = sum(c.sometimes_excluded_lines for c in evaluated)
    excluded_files = {c.path for c in evaluated if c.always_excluded_lines}
    return {
        "tus": tus,
        "headers": headers,
        "files": len(corpus.files),
        "total_lines": total_lines,
        "chains": len(chains),
        "evaluated": len(evaluated),
        "never": len(never),
        "guards": len(guards),
        "unterminated": sum(1 for c in chains if not c.terminated),
        "always": always,
        "sometimes": sometimes,
        "excluded_files": len(excluded_files),
        "multi_run_headers": sum(1 for k, _, r in corpus.files.values() if k == "header" and r > 1),
    }


def class_table(corpus: Corpus, stats: dict[str, NameStats]) -> list[str]:
    rows: dict[str, list[int]] = {c: [0, 0, 0, 0, 0] for c in CLASSES}
    for name, s in stats.items():
        r = rows[cls_of(corpus, name)]
        r[0] += 1
        r[1] += s.chains
        r[2] += s.sole
        r[3] += s.shared
        r[4] += s.unbound
    out = [
        "| Class | Names | Chains reading them | Lines excluded, sole dependency | Lines excluded, shared | Unbound reads |",
        "|-------|------:|--------------------:|-------------------------------:|-----------------------:|--------------:|",
    ]
    for c in CLASSES:
        r = rows[c]
        out.append(f"| {c} | {fmt(r[0])} | {fmt(r[1])} | {fmt(r[2])} | {fmt(r[3])} | {fmt(r[4])} |")
    return out


def direction_table(corpus: Corpus) -> list[str]:
    evaluated = [c for c in corpus.chains.values() if c.evaluated and not c.guard]
    first_never = [c for c in evaluated if c.arms[0].always_excluded]
    first_always = [c for c in evaluated if c.arms[0].skipped == 0 and c.arms[0].taken > 0]
    first_mixed = [c for c in evaluated if c.arms[0].sometimes_excluded]
    else_taken = [
        c for c in first_never
        if any(a.directive == "else" and a.skipped == 0 and a.taken > 0 for a in c.arms)
    ]
    no_arm = [
        c for c in first_never
        if all(a.taken == 0 for a in c.arms)
    ]
    by_directive: Counter[str] = Counter()
    lines_by_directive: Counter[str] = Counter()
    for c in evaluated:
        for a in c.arms:
            if a.always_excluded:
                by_directive[a.directive] += 1
                lines_by_directive[a.directive] += a.body_lines
    out = [
        "| Chains (guards excluded) | Count |",
        "|--------------------------|------:|",
        f"| Evaluated by at least one run | {fmt(len(evaluated))} |",
        f"| First arm never taken | {fmt(len(first_never))} |",
        f"| … of which an `#else` arm was always taken | {fmt(len(else_taken))} |",
        f"| … of which no arm was ever taken (no `#else`, or every arm false) | {fmt(len(no_arm))} |",
        f"| First arm always taken | {fmt(len(first_always))} |",
        f"| First arm taken in some runs, not in others | {fmt(len(first_mixed))} |",
        "",
        "| Always-excluded arms by directive | Arms | Lines |",
        "|-----------------------------------|-----:|------:|",
    ]
    for d in ["if", "ifdef", "ifndef", "elif", "else"]:
        out.append(f"| `#{d}` | {fmt(by_directive[d])} | {fmt(lines_by_directive[d])} |")
    return out


def expression_table(corpus: Corpus) -> list[str]:
    groups: dict[str, list[Chain]] = defaultdict(list)
    for c in corpus.chains.values():
        if c.evaluated and not c.guard:
            groups[c.key].append(c)
    ranked = sorted(
        groups.items(),
        key=lambda kv: (-sum(c.always_excluded_lines for c in kv[1]), kv[0]),
    )
    out = [
        "| Expression (chain as written) | Regions | Files | Lines always excluded | Lines sometimes excluded | Names (class) |",
        "|-------------------------------|--------:|------:|----------------------:|-------------------------:|---------------|",
    ]
    for key, chains in ranked[:TOP_N]:
        always = sum(c.always_excluded_lines for c in chains)
        if always == 0:
            break
        sometimes = sum(c.sometimes_excluded_lines for c in chains)
        files = len({c.path for c in chains})
        names: list[str] = []
        for c in chains:
            for n in c.names:
                if n not in names:
                    names.append(n)
        named = ", ".join(f"{code(n)} ({cls_of(corpus, n)})" for n in names[:6])
        if len(names) > 6:
            named += f", … ({len(names) - 6} more)"
        out.append(
            f"| {code(key)} | {fmt(len(chains))} | {fmt(files)} | {fmt(always)} | {fmt(sometimes)} | {named} |"
        )
    return out


def name_rows(corpus: Corpus, stats: dict[str, NameStats], names: list[str]) -> list[str]:
    out = [
        "| Name | Class | Chains | Lines, sole | Lines, shared | Lines, sometimes | Reads bound / unbound | In-tree `#define` | Build file |",
        "|------|-------|-------:|------------:|--------------:|-----------------:|----------------------:|-------------------|------------|",
    ]
    for n in names:
        s = stats[n]
        info = corpus.names.get(n)
        define = "—"
        build = "—"
        if info:
            if info.cli:
                define = "`-D`"
            elif info.defines:
                define = f"{code(info.define_site)}" + (f" (+{info.defines - 1})" if info.defines > 1 else "")
            if info.build_file != "-":
                build = code(info.build_file)
        out.append(
            f"| {code(n)} | {cls_of(corpus, n)} | {fmt(s.chains)} | {fmt(s.sole)} | {fmt(s.shared)} | "
            f"{fmt(s.sometimes)} | {fmt(s.bound)} / {fmt(s.unbound)} | {define} | {build} |"
        )
    return out


def gn_candidate_table(corpus: Corpus, stats: dict[str, NameStats]) -> list[str]:
    out = [
        "| Name | Value | Source | Confidence | GN conditions | Lines, sole / shared |",
        "|------|-------|--------|------------|---------------|----------------------:|",
    ]
    # Lines are per name, so weigh each name once rather than per entry.
    weight: dict[str, int] = {}
    for c in corpus.gn_candidates:
        s = stats.get(c.name, NO_STATS)
        weight[c.name] = -(s.sole + s.shared)
    ordered = sorted(corpus.gn_candidates,
                     key=lambda c: (RANKS[c.confidence], weight[c.name], c.name, c.path, c.line))
    for c in ordered[:TOP_N]:
        s = stats.get(c.name, NO_STATS)
        value = "(no explicit value)" if c.value is None else (code(c.value) if c.value else "(empty)")
        # A name no chain reads has no line evidence at all, which is not the
        # same as a name whose chains happen to exclude no lines.
        lines = f"{fmt(s.sole)} / {fmt(s.shared)}" if c.name in stats else "—"
        out.append(f"| {code(c.name)} | {value} | {code(f'{c.path}:{c.line}')} | "
                   f"{c.confidence} | {code(c.conditions) if c.conditions else '—'} | "
                   f"{lines} |")
    return out


def render_corpus(corpus_meta: dict, corpus: Corpus) -> list[str]:
    name = corpus_meta["name"]
    s = summarize(corpus)
    stats = name_stats(corpus)
    lines: list[str] = []
    lines.append(f"## {name}")
    lines.append("")
    defines = corpus.defines.strip() or "(none)"
    lines.append(
        f"Generated from `conditional_coverage {corpus_meta['root']}` with `-D` defines: {defines}."
    )
    lines.append("")
    lines.append("| | |")
    lines.append("|---|---:|")
    lines.append(f"| Files preprocessed (translation units + headers) | {fmt(s['files'])} ({fmt(s['tus'])} + {fmt(s['headers'])}) |")
    lines.append(f"| Headers evaluated by more than one unit | {fmt(s['multi_run_headers'])} |")
    lines.append(f"| Source lines | {fmt(s['total_lines'])} |")
    lines.append(f"| Conditional chains | {fmt(s['chains'])} |")
    lines.append(f"| … include guards | {fmt(s['guards'])} |")
    lines.append(f"| … evaluated by at least one run | {fmt(s['evaluated'])} |")
    lines.append(f"| … never evaluated (inside excluded code in every run) | {fmt(s['never'])} |")
    lines.append(f"| … left unterminated | {fmt(s['unterminated'])} |")
    lines.append(f"| **Lines always excluded** | **{fmt(s['always'])} ({pct(s['always'], s['total_lines'])})** |")
    lines.append(f"| Lines excluded in some runs, included in others | {fmt(s['sometimes'])} ({pct(s['sometimes'], s['total_lines'])}) |")
    lines.append(f"| Files with an always-excluded arm | {fmt(s['excluded_files'])} |")
    lines.append("")
    lines.append("### Which arm is taken")
    lines.append("")
    lines.extend(direction_table(corpus))
    lines.append("")
    lines.append("### Excluded lines by name class")
    lines.append("")
    lines.extend(class_table(corpus, stats))
    lines.append("")
    lines.append("### Top expressions by lines always excluded")
    lines.append("")
    lines.extend(expression_table(corpus))
    lines.append("")
    lines.append("### Names, derived view")
    lines.append("")
    lines.append(
        "Lines are apportioned as described above: *sole* when the name is the only one the chain "
        "reads, *shared* otherwise (a shared line is listed under every name of its chain)."
    )
    lines.append("")
    ranked = sorted(
        stats,
        key=lambda n: (-(stats[n].sole + stats[n].shared), -stats[n].sometimes, n),
    )
    top = [n for n in ranked if stats[n].sole + stats[n].shared + stats[n].sometimes > 0][:TOP_N]
    lines.extend(name_rows(corpus, stats, top))
    lines.append("")
    lines.append("### Unknown names")
    lines.append("")
    unknown = [n for n in ranked if cls_of(corpus, n) == "unknown"]
    lines.append(
        f"{fmt(len(unknown))} names nothing in the checkout accounts for; "
        f"{fmt(sum(1 for n in unknown if stats[n].sole + stats[n].shared > 0))} of them control an always-excluded arm. "
        f"Top {min(TOP_N, len(unknown))} by attributable lines:"
    )
    lines.append("")
    lines.extend(name_rows(corpus, stats, unknown[:TOP_N]))
    lines.append("")
    lines.append("### GN define candidates")
    lines.append("")
    lines.append(
        "Candidates only: never applied, not per-TU configuration, and conditions are recorded "
        "rather than evaluated. Confidence levels, what is skipped and what is not resolved are "
        "defined in [GN_DEFINES.md](GN_DEFINES.md). Ranked by confidence, then by lines in "
        "always-excluded chains reading the name (shared lines overlap and are not predicted "
        "recovery); `—` in that column marks a name no conditional chain reads at all, as "
        "against `0 / 0` for one that is read but gates no always-excluded lines. "
        f"Showing {min(TOP_N, len(corpus.gn_candidates))} of "
        f"{fmt(len(corpus.gn_candidates))} candidate entries from `BUILD.gn`, `*.gni` and `*.gn`; "
        "the TSV retains every entry."
    )
    lines.append("")
    lines.extend(gn_candidate_table(corpus, stats))
    lines.append("")
    return lines


def main() -> int:
    parsed = [(c, load_tsv(c["tsv"])) for c in CORPORA]

    out: list[str] = []
    out.append("# Conditional-compilation coverage — eval corpora")
    out.append("")
    out.append(
        "What the single default configuration excludes (#57): every `#if` / `#ifdef` / "
        "`#ifndef` chain, which arm each preprocess run took, the source lines in the arms "
        "not taken, and what the checkout knows about the names the conditions read. "
        "Reporting only — preprocessing behaviour is unchanged. Regenerate with:"
    )
    out.append("")
    out.append("```bash")
    out.append("set -euo pipefail   # stop at the first failure, do not run on with stale inputs")
    out.append("")
    out.append(f"export {CORPUS_ENV}={shlex.quote(str(CORPUS_BASE))}")
    out.append("python3 scripts/fetch_corpora.py   # corpora at the revisions pinned in scripts/eval_expected.json")
    out.append("cargo build --release -p trace-cli --examples")
    out.append("")
    out.append("# Each TSV is written to a .part file and renamed only if the command")
    out.append("# succeeded; the generator treats a MISSING file as an error.")
    out.append("rm -f /tmp/conditional_coverage_{hdf,hiview,camera}.tsv{,.part}")
    for c in CORPORA:
        root = f'"${CORPUS_ENV}/{c["name"]}"'
        out.append(f"target/release/examples/conditional_coverage {root} > {c['tsv']}.part")
        out.append(f"mv {c['tsv']}.part {c['tsv']}")
    out.append("")
    out.append("python3 scripts/gen_conditional_coverage_report.py")
    out.append("```")
    out.append("")
    out.append("## How to read this")
    out.append("")
    out.append(
        "- **The record is the chain, not the macro.** A chain is one `#if`/`#ifdef`/`#ifndef` "
        "with its `#elif`/`#else` arms. The lines an arm excludes belong to the whole expression "
        "that controls the chain; for `#if A && B` crediting them to `A` and to `B` separately "
        "double-counts and overstates what defining either one would recover. The per-name view "
        "below therefore splits lines into *sole* (the name is the chain's only dependency) and "
        "*shared* (listed under every name of the chain)."
    )
    out.append(
        "- **Environment.** Every translation unit is preprocessed from the command-line defines "
        "alone with its includes expanded inline — the environment `trace analyze` gives each "
        "unit — and headers no unit reaches are preprocessed standalone, as the indexer does with "
        "orphans. No expansion cache: a cache hit replays a header's text without re-evaluating "
        "its conditionals. A header reached from several units is evaluated once per unit, so an "
        "arm can be taken in some runs and not in others (*sometimes excluded*); *always excluded* "
        "arms were never taken by any run. File totals include headers resolved outside the root "
        "through `--include`. Missing or empty source trees and hard input failures stop the "
        "measurement without publishing TSV. Command-line metadata retains `-D` values. "
        "A final completion record counts all preceding TSV rows; missing or mismatched "
        "completion records are rejected, including captures cut off at a complete line. "
        "Older TSV files must be regenerated."
    )
    out.append(
        "- **Which arm.** An undefined name does not always select `#else`: `#if !X` with `X` "
        "unknown takes the first arm. Each arm's outcome is recorded per run rather than assumed."
    )
    out.append(
        "- **Names read** are what the evaluation consulted, macro expansion included "
        "(`#if HAS_X` with `#define HAS_X defined(X)` reads both). *Unbound reads* count the "
        "evaluations that found no macro bound to the name — the cases that resolved against the "
        "default of `0`. An arm that was never evaluated contributes only the identifiers it spells."
    )
    out.append(
        "- **Classes.** *include-guard*: tested by a chain that wraps a whole file (`#ifndef X` "
        "first, `#define X` next, no `#else`, nothing after its `#endif`) and by no other chain — "
        "a default-value idiom alone in a file has the guard shape, and an `#if X > 1` elsewhere "
        "that depends on the name says it is configuration. *toolchain*: a macro gcc/clang "
        "predefine (a fixed list — language, compiler, target OS, architecture, type sizes, "
        "`__has_*`). *configuration*: a `-D`, an in-tree `#define` in any region (comments and "
        "string literals ignored), or a name an "
        "in-tree build file spells (GN, CMake, Make, Kconfig — spelled, not parsed). "
        "Separately, GN define candidates (#58) record direct string entries in `defines = [...]` "
        "and `defines += [...]` in `BUILD.gn`, `*.gni` and `*.gn`, with values, entry locations, "
        "conditions and confidence. Computed entries and interpolated names are skipped. "
        "*unknown*: nothing in the checkout accounts for it. Unknown is a real "
        "category, not a failure to classify: #59 needs to know which names it cannot reason about."
    )
    out.append(
        "- **Lines are source lines** strictly between the arm's directive and the next directive "
        "of its chain, not reachable code: a nested chain's directive lines count, blank and "
        "comment lines and continuation lines of multiline conditions count, and an always-excluded outer arm hides its inner chains (they are "
        "*never evaluated* and add nothing). Sometimes-excluded lines of nested chains can overlap. "
        "Excluded lines are not an acceptance metric on their own — what matters for #59 is "
        "whether the excluded arms hold new, source-verified driver and callback targets."
    )
    out.append("")
    out.append("## Overview")
    out.append("")
    out.append(
        "| Corpus | Files | Source lines | Chains | Always excluded | Sometimes excluded | Unknown names | … controlling excluded arms |"
    )
    out.append(
        "|--------|------:|-------------:|-------:|----------------:|-------------------:|--------------:|----------------------------:|"
    )
    for c, corpus in parsed:
        s = summarize(corpus)
        stats = name_stats(corpus)
        unknown = [n for n in stats if cls_of(corpus, n) == "unknown"]
        unknown_excl = sum(1 for n in unknown if stats[n].sole + stats[n].shared > 0)
        out.append(
            f"| `{c['name']}` | {fmt(s['files'])} | {fmt(s['total_lines'])} | {fmt(s['chains'])} | "
            f"{fmt(s['always'])} ({pct(s['always'], s['total_lines'])}) | "
            f"{fmt(s['sometimes'])} ({pct(s['sometimes'], s['total_lines'])}) | "
            f"{fmt(len(unknown))} | {fmt(unknown_excl)} |"
        )
    out.append("")
    out.append("## Always-excluded lines by name class, sole dependency")
    out.append("")
    out.append("| Class | HDF | Hiview | Camera |")
    out.append("|-------|----:|-------:|-------:|")
    per = []
    for _, corpus in parsed:
        stats = name_stats(corpus)
        row: Counter[str] = Counter()
        for n, s in stats.items():
            row[cls_of(corpus, n)] += s.sole
        per.append(row)
    for cls in CLASSES:
        out.append(f"| {cls} | " + " | ".join(fmt(r[cls]) for r in per) + " |")
    out.append("")

    for c, corpus in parsed:
        out.extend(render_corpus(c, corpus))
        out.append("---")
        out.append("")

    OUT.write_text("\n".join(out).rstrip() + "\n")
    print(f"Wrote {OUT} ({len(parsed)} corpora, {sum(len(x.chains) for _, x in parsed)} chains)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
