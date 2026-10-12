#!/usr/bin/env python3
"""Build, train, and compare native ThinLTO/PGO trace binaries.

Uses the pinned eval corpora as training data and optional clean Git trees as
holdouts. Artifacts and logs go to a fresh directory under target/pgo by default.
See docs/PGO.md. Requires Python 3.10+ (3.11+ with Cargo config files), Cargo,
and rustc's llvm-tools-preview.
"""

import argparse
from contextlib import closing
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time

from fetch_corpora import corpus_base, git_output

ROOT = Path(__file__).resolve().parent.parent


def resolve_path(value):
    return Path(value).expanduser().resolve()


def paths_overlap(left, right):
    left, right = resolve_path(left), resolve_path(right)
    return left == right or left in right.parents or right in left.parents


def output(command, cwd=ROOT, env=None):
    return subprocess.check_output(command, cwd=cwd, env=env, text=True).strip()


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_identity(exclude=None):
    # Include untracked source files too; a dirty development checkout is valid,
    # but all builds must consume exactly the same source snapshot.
    paths = ["--", "."]
    if exclude is not None and exclude.is_relative_to(ROOT):
        paths.append(":(exclude,literal)" + str(exclude.relative_to(ROOT)))
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", *paths], cwd=ROOT
    ).decode().split("\0")
    digest = hashlib.sha256()
    for name in sorted(set(names) - {""}):
        path = ROOT / name
        digest.update(name.encode() + b"\0")
        digest.update(file_hash(path).encode() if path.is_file() else b"missing")
    return {"revision": git_output(["rev-parse", "HEAD"], ROOT),
            "status": git_output(["status", "--porcelain", *paths], ROOT),
            "sha256": digest.hexdigest()}


def input_fingerprint(top, names, kind):
    digest = hashlib.sha256()
    names = sorted(set(names) - {b""})
    for name in names:
        entry = Path(top) / os.fsdecode(name)
        # Git may report a nested repository as a directory. Directory links
        # could also hide included headers: neither has enumerable file content.
        if entry.is_dir() or not (entry.is_file() or entry.is_symlink()):
            raise ValueError(f"cannot snapshot {kind} corpus input: {entry}")
        digest.update(name + b"\0")
        link = os.fsencode(os.readlink(entry)) if entry.is_symlink() else b""
        digest.update(link + b"\0")
        if entry.is_symlink():
            digest.update(os.fsencode(entry.resolve()) + b"\0")
        digest.update(file_hash(entry).encode() if entry.is_file() else b"missing")
    return {"files": len(names), "sha256": digest.hexdigest()}


def checkout(path, revision=None):
    path = resolve_path(path)
    if not path.is_dir():
        raise ValueError(f"missing corpus: {path}; run scripts/fetch_corpora.py")
    top = git_output(["rev-parse", "--show-toplevel"], path)
    head = git_output(["rev-parse", "HEAD"], path)
    if not top or not head:
        raise ValueError(f"corpus must belong to a Git checkout: {path}")
    relative = path.relative_to(Path(top).resolve())
    scope = ":(literal)" + str(relative)
    status = git_output(["status", "--porcelain", "--untracked-files=all",
                         "--ignore-submodules=none", "--", scope], Path(top))
    if status is None or status:
        raise ValueError(f"corpus has local changes or cannot be checked: {path}")
    if revision and head != revision:
        raise ValueError(f"{path}: revision {head}, expected {revision}")
    entries = subprocess.check_output(
        ["git", "ls-files", "--stage", "-z", "--", scope], cwd=top,
    ).split(b"\0")
    symlinks = []
    for entry in filter(None, entries):
        metadata, name = entry.split(b"\t", 1)
        mode = metadata.split(b" ", 1)[0]
        if mode == b"160000":
            raise ValueError(f"corpus contains tracked submodule: {Path(top) / os.fsdecode(name)}; "
                             "select the submodule as a separate corpus")
        if mode == b"120000":
            symlinks.append(name)
    # Analysis does not respect Git ignores. Include all ignored files rather
    # than duplicating its source/header/configuration discovery rules here.
    names = subprocess.check_output(
        ["git", "ls-files", "--others", "--ignored", "--exclude-standard", "-z", "--", scope],
        cwd=top,
    ).split(b"\0")
    return {"path": str(path), "revision": head,
            "ignored_inputs": input_fingerprint(top, names, "ignored"),
            "symlink_inputs": input_fingerprint(top, symlinks, "symlink")}


def verify_checkout(snapshot):
    current = checkout(snapshot["path"], snapshot["revision"])
    if any(current[key] != snapshot[key] for key in current):
        raise RuntimeError(f"corpus inputs changed during experiment: {snapshot['path']}")


def conflicting_codegen_flags(flags):
    forbidden = {"profile-generate", "profile-use", "lto", "codegen-units"}
    for index, flag in enumerate(flags):
        if flag in ("-C", "--codegen"):
            option = flags[index + 1] if index + 1 < len(flags) else ""
        elif flag.startswith("-C"):
            option = flag[2:].removeprefix("=")
        elif flag.startswith("--codegen="):
            option = flag[len("--codegen="):]
        else:
            continue
        if option.split("=", 1)[0].replace("_", "-") in forbidden:
            return True
    return False


def cargo_settings(env, root):
    """Resolve the compiler; reject flags whose Cargo merging we don't model."""
    root = Path(root).absolute()
    home = Path(env.get("CARGO_HOME") or Path(env.get("HOME", str(Path.home()))) / ".cargo")
    if not home.is_absolute():
        home = root / home
    directories = [directory / ".cargo" for directory in (root, *root.parents)] + [home]
    configs = []
    seen = set()
    for directory in directories:
        # Cargo prefers the legacy filename when both exist.
        config = directory / "config"
        if not config.is_file():
            config = directory / "config.toml"
        if not config.is_file() or config in seen:
            continue
        seen.add(config)
        try:
            import tomllib
        except ImportError:
            raise ValueError("reading Cargo configuration requires Python 3.11+; "
                             f"found {config}") from None
        data = tomllib.loads(config.read_text(encoding="utf-8"))
        if "include" in data:
            raise ValueError(f"Cargo config includes are unsupported: {config}")
        configs.append((config, data))

    if "CARGO_ENCODED_RUSTFLAGS" in env:
        flags = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        if flags == [""]:
            flags = []
    elif "RUSTFLAGS" in env:
        # Cargo splits this variable on whitespace, preserving literal quotes.
        flags = env["RUSTFLAGS"].split()
    else:
        for key in env:
            if key == "CARGO_BUILD_RUSTFLAGS" or (
                    key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS")):
                raise ValueError(f"unsupported Cargo-configured Rust flags in {key}; "
                                 "pass effective flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS")
        for config, data in configs:
            build_flags = "rustflags" in data.get("build", {})
            target_flags = any("rustflags" in value for value in data.get("target", {}).values()
                               if isinstance(value, dict))
            if build_flags or target_flags:
                raise ValueError(f"unsupported Cargo-configured Rust flags in {config}; "
                                 "pass effective flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS")
        flags = []
    if conflicting_codegen_flags(flags):
        raise ValueError("remove PGO/LTO/codegen-units overrides from inherited RUSTFLAGS")

    compiler = env.get("RUSTC", env.get("CARGO_BUILD_RUSTC"))
    origin = root
    if compiler is None:
        for config, data in configs:
            if "rustc" in data.get("build", {}):
                compiler = data["build"]["rustc"]
                origin = config.parent.parent
                break
    if compiler is None:
        compiler = "rustc"
    if not isinstance(compiler, str) or not compiler:
        raise ValueError("Cargo compiler must be a nonempty executable path")
    if "/" in compiler and not Path(compiler).is_absolute():
        # Preserve rustup proxy symlinks: resolving one changes argv[0].
        compiler = os.path.normpath(origin / compiler)
    return {"compiler": compiler, "rustflags": flags,
            "config_files": [{"path": str(config), "sha256": file_hash(config)}
                             for config, _ in configs]}


def quote_identifier(name):
    return '"' + name.replace('"', '""') + '"'


def database_fingerprint(path):
    """Hash every sorted row, preserving duplicates and schema, in bounded memory.

    Keep analysis_run.options_json, including convergence and solver pop counts.
    Only timestamp and binary version are excluded from comparisons.
    """
    path = resolve_path(path)
    result = {}
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as conn:
        tables = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").fetchall()
        for (name,) in tables:
            table = quote_identifier(name)
            schema = [list(row) for row in conn.execute(f"PRAGMA table_info({table})")]
            columns = [c[1] for c in schema
                       if not (name == "analysis_run" and c[1] in ("created_at", "trace_version"))]
            fields = ", ".join(quote_identifier(c) for c in columns)
            digest = hashlib.sha256()
            count = 0
            for row in conn.execute(f"SELECT {fields} FROM {table} ORDER BY {fields}"):
                # repr preserves SQLite NULL, text, BLOB and numeric distinctions.
                digest.update(repr(row).encode() + b"\n")
                count += 1
            result[name] = {"schema": schema, "rows": count, "sha256": digest.hexdigest()}
    return result


def require_equal(expected, actual, label):
    changed = [name for name in sorted(expected.keys() | actual.keys())
               if expected.get(name) != actual.get(name)]
    if changed:
        raise RuntimeError(f"analysis differs for {label}: {', '.join(changed)}")


def parse_measurement(log, wall):
    number = r"\d+(?:[.,]\d+)?"

    def decimal(value):
        return float(value.replace(",", "."))

    rss = re.search(r"TRACE_PGO_RSS_KIB=(\d+)", log)
    if rss:
        peak = int(rss[1]) * 1024
        user_time = re.search(rf"TRACE_PGO_USER_S=({number})", log)
        system_time = re.search(rf"TRACE_PGO_SYSTEM_S=({number})", log)
        if not user_time or not system_time:
            raise RuntimeError("missing /usr/bin/time measurement")
        user, system = decimal(user_time[1]), decimal(system_time[1])
        measured_wall = re.search(rf"TRACE_PGO_WALL_S=({number})", log)
        if measured_wall:
            wall = decimal(measured_wall[1])
    else:
        rss = re.search(r"(\d+)\s+maximum resident set size", log)
        cpu = re.search(rf"({number})\s+real\s+({number})\s+user\s+({number})\s+sys", log)
        if not rss or not cpu:
            raise RuntimeError("missing /usr/bin/time measurement")
        wall, peak, user, system = decimal(cpu[1]), int(rss[1]), decimal(cpu[2]), decimal(cpu[3])
    phases = {name + "_s": decimal(value) for name, value in re.findall(
        rf"^(index|analyze|export): ({number})s", log, re.MULTILINE)}
    if len(phases) != 3:
        raise RuntimeError("missing trace phase measurements")
    if wall <= 0:
        raise RuntimeError("analysis is too short for /usr/bin/time resolution; use a larger workload")
    return {"wall_s": wall, "peak_rss_bytes": peak,
            "user_s": user, "system_s": system, **phases}


def stop_analysis(proc):
    # The group can disappear between wait timing out and delivery of SIGKILL.
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait()


def run_analysis(binary, case, db, log, env, jobs, budget, timeout, measure=False):
    # Resolve relative output names against this caller's cwd, preserving a
    # final-component symlink so unlink removes the entry rather than its target.
    db, log = Path(db).expanduser().absolute(), Path(log).expanduser().absolute()
    # An exit code of zero is insufficient: this invocation must publish its DB.
    db.unlink(missing_ok=True)
    run_env = env.copy()
    run_env["LC_ALL"] = "C"
    command = [str(binary), "analyze", case["path"], "--require-local-inputs", "--jobs", str(jobs),
               "--solve-budget-pops", str(budget), "-o", str(db), *case["args"]]
    if measure:
        prefix = (["/usr/bin/time", "-f",
                   "TRACE_PGO_RSS_KIB=%M TRACE_PGO_USER_S=%U TRACE_PGO_SYSTEM_S=%S TRACE_PGO_WALL_S=%e"]
                  if sys.platform == "linux" else ["/usr/bin/time", "-l"])
        command = prefix + command
    start = time.perf_counter()
    with log.open("w", encoding="utf-8") as stream:
        with subprocess.Popen(command, cwd=ROOT, env=run_env, stdout=stream,
                              stderr=subprocess.STDOUT, start_new_session=True) as proc:
            try:
                proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                # /usr/bin/time is a wrapper: stop its analyzer child too.
                stop_analysis(proc)
                raise RuntimeError(f"analysis timed out after {timeout}s; see {log}") from None
            except KeyboardInterrupt:
                stop_analysis(proc)
                raise
    elapsed = time.perf_counter() - start
    if proc.returncode:
        raise RuntimeError(f"analysis failed ({proc.returncode}); see {log}")
    if not db.is_file():
        raise RuntimeError(f"analysis produced no fresh database; see {log}")
    if measure:
        return parse_measurement(log.read_text(encoding="utf-8", errors="replace"), elapsed)
    return elapsed


def build(label, lto, profile_flag, work, target, args, env):
    build_env = env.copy()
    # Explicit --target isolates runtime RUSTFLAGS from build scripts/proc macros.
    encoded = env.get("CARGO_ENCODED_RUSTFLAGS", "")
    flags = encoded.split("\x1f") if encoded else []
    # Cargo package profile overrides outrank the global profile environment.
    # Runtime Rust flags follow Cargo's generated flags, so this wins for each crate.
    flags.append("-Ccodegen-units=1")
    if profile_flag:
        flags.append(profile_flag)
    if profile_flag and profile_flag.startswith("-Cprofile-use="):
        flags.append("-Cllvm-args=-pgo-warn-missing-function")
    build_env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    build_env["CARGO_PROFILE_RELEASE_LTO"] = lto
    build_env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] = "1"
    build_env["CARGO_INCREMENTAL"] = "0"
    command = ["cargo", "build", "--locked", "--release", "-p", "trace-cli", "--bin", "trace",
               "--target", target, "--target-dir", str(work / label),
               "--jobs", str(args.build_jobs)]
    if args.features:
        command += ["--features", args.features]
    log = work / f"{label}-build.log"
    print(f"Building {label} (LTO={lto}); log: {log}", flush=True)
    start = time.perf_counter()
    with log.open("w", encoding="utf-8") as stream:
        proc = subprocess.run(command, cwd=ROOT, env=build_env,
                              stdout=stream, stderr=subprocess.STDOUT)
    if proc.returncode:
        raise RuntimeError(f"build failed ({proc.returncode}); see {log}")
    binary = work / label / target / "release/trace"
    return {"path": str(binary), "sha256": file_hash(binary),
            "bytes": binary.stat().st_size, "build_wall_s": time.perf_counter() - start,
            "command": command, "rustflags": flags, "lto": lto}


def summarize(samples):
    return {key: {"median": statistics.median(s[key] for s in samples),
                  "min": min(s[key] for s in samples), "max": max(s[key] for s in samples)}
            for key in samples[0]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus-base", help="default $TRACE_CORPUS_BASE or ~")
    parser.add_argument("--expected", type=resolve_path, default=ROOT / "scripts/eval_expected.json")
    parser.add_argument("--train", nargs="+", default=["hdf", "hiview", "camera"],
                        help="pinned corpus names used for training")
    parser.add_argument("--holdout", type=resolve_path, action="append", default=[],
                        help="clean Git tree/subtree excluded from training (repeatable)")
    parser.add_argument("--fat-lto", action="store_true", help="also compare fat LTO with/without PGO")
    parser.add_argument("--features", default="", help="trace-cli Cargo features, e.g. mimalloc")
    parser.add_argument("--jobs", type=int, default=8, help="index workers for all analyses")
    parser.add_argument("--build-jobs", type=int, default=2)
    parser.add_argument("--runs", type=int, default=5, help="measurement runs per binary/case")
    parser.add_argument("--budget", type=int, help="solver pop budget (default pinned eval budget)")
    parser.add_argument("--timeout", type=int, default=1800, help="seconds per analysis")
    parser.add_argument("--outdir", type=resolve_path, help="new directory; default fresh target/pgo/run-*")
    parser.add_argument("--llvm-profdata", type=resolve_path, help="override matching rustc LLVM tool")
    parser.add_argument("--training-modes", nargs="+", choices=["minimal", "full", "explore"],
                        default=["minimal"], help="also benchmark these modes")
    args = parser.parse_args()
    if sys.platform not in ("linux", "darwin"):
        parser.error("this experiment supports native Linux and macOS builds")
    if min(args.jobs, args.build_jobs, args.runs, args.timeout) < 1:
        parser.error("jobs, build-jobs, runs, and timeout must be positive")
    if args.budget is not None and args.budget < 0:
        parser.error("budget must be nonnegative (0 is unlimited)")
    if not Path("/usr/bin/time").is_file():
        parser.error("/usr/bin/time is required")
    expected = json.loads(args.expected.read_text(encoding="utf-8"))
    if len(set(args.train)) != len(args.train):
        parser.error("training corpus names must be unique")
    unknown = set(args.train) - expected["corpora"].keys()
    if unknown:
        parser.error(f"unknown training corpora: {sorted(unknown)}")
    base = corpus_base(args.corpus_base)
    train = [{"name": name, **checkout(base / expected["corpora"][name]["dir"],
                                      expected["corpora"][name]["rev"])} for name in args.train]
    holdouts = [{"name": f"holdout-{n}-{path.name}", **checkout(path)}
                for n, path in enumerate(args.holdout)]
    for held in holdouts:
        h = Path(held["path"])
        if any(paths_overlap(h, t["path"]) for t in train):
            parser.error(f"holdout overlaps training data: {h}")
    destination = args.outdir or resolve_path(ROOT / "target/pgo")
    if any(paths_overlap(destination, c["path"]) for c in train + holdouts):
        parser.error("artifact directory must not contain or be inside any corpus tree")
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    try:
        settings = cargo_settings(env, ROOT)
    except ValueError as exc:
        parser.error(str(exc))
    compiler = settings["compiler"]
    env["RUSTC"] = compiler
    flags = settings["rustflags"]
    env.pop("RUSTFLAGS", None)
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    env.pop("LLVM_PROFILE_FILE", None)
    rustc = output([compiler, "-Vv"], cwd=ROOT, env=env)
    target = re.search(r"^host: (.+)$", rustc, re.MULTILINE)[1]
    llvm_version = re.search(r"^LLVM version: (.+)$", rustc, re.MULTILINE)[1]
    profdata = resolve_path(
        args.llvm_profdata or Path(output([compiler, "--print", "sysroot"], cwd=ROOT, env=env))
        / "lib/rustlib" / target / "bin/llvm-profdata")
    if not profdata.is_file():
        if args.llvm_profdata is not None:
            parser.error(f"--llvm-profdata is not an existing file: {profdata}")
        parser.error(f"matching llvm-profdata missing at {profdata}; "
                     "run rustup component add llvm-tools-preview")
    profdata_version = output([str(profdata), "--version"])
    version = re.search(r"(?:LLVM version|version)\s+(\d+)\.", profdata_version)
    if not version or version[1] != llvm_version.split(".")[0]:
        parser.error(f"llvm-profdata must match rustc LLVM {llvm_version}: {profdata_version}")
    budget = args.budget if args.budget is not None else expected["budget_pops"]
    env["TRACE_SOLVE_BUDGET_POPS"] = str(budget)
    source = source_identity()
    env["TRACE_BUILD_GIT_SHA"] = source["revision"][:7]
    env["TRACE_BUILD_GIT_DIRTY"] = str(bool(source["status"])).lower()
    env["TRACE_BUILD_DATE"] = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    if args.outdir:
        work = destination
        work.mkdir(parents=True, exist_ok=False)
    else:
        parent = destination
        parent.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    print(f"Experiment artifacts: {work}", flush=True)
    cases = [{**corpus, "role": role, "mode": mode,
              "name": corpus["name"] + "-" + mode,
              "args": {"minimal": [], "full": ["--full-export"],
                       "explore": ["--explore", "--explore-budget", "4"]}[mode]}
             for role, corpora in [("training", train), ("holdout", holdouts)]
             for corpus in corpora for mode in dict.fromkeys(args.training_modes)]
    report = {"status": "running", "measured_at": datetime.now(timezone.utc).isoformat(),
              "platform": platform.platform(), "machine": platform.machine(), "cpu_count": os.cpu_count(),
              "rustc": rustc, "cargo": output(["cargo", "-V"]),
              "cargo_settings": settings,
              "llvm_profdata": {"path": str(profdata), "version": profdata_version},
              "source": source, "expected_sha256": file_hash(args.expected),
              "jobs": args.jobs, "solver_budget_pops": budget, "features": args.features,
              "require_local_inputs": True,
              "runs": args.runs, "cases": cases, "builds": {}, "profiles": {}, "comparisons": [],
              "environment": {k: v for k, v in env.items() if k.startswith(("TRACE_", "CARGO_PROFILE_"))
                              or k in ("CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                                       "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTUP_TOOLCHAIN",
                                       "LC_ALL")},
              "measurement": "warm filesystem caches; fresh processes; /usr/bin/time wall (0.01s); phases (0.1s)"}
    report_path = work / "report.json"

    def save():
        report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    save()
    try:
        for lto in (["thin", "fat"] if args.fat_lto else ["thin"]):
            report["builds"][lto] = build(lto, lto, None, work, target, args, env)
            save()
            raw = work / f"{lto}-profiles"
            raw.mkdir()
            instrument = build(lto + "-instrumented", lto, f"-Cprofile-generate={raw}",
                               work, target, args, env)
            report["builds"][lto + "-instrumented"] = instrument
            train_env = env.copy()
            train_env["LLVM_PROFILE_FILE"] = str(raw / "%m-%p.profraw")
            for case in cases:
                if case["role"] == "training":
                    print(f"Training {lto}: {case['name']}", flush=True)
                    with tempfile.TemporaryDirectory(prefix="trace-pgo-train-") as scratch:
                        run_analysis(Path(instrument["path"]), case, Path(scratch) / "analysis.db",
                                     work / f"{lto}-train-{case['name']}.log", train_env,
                                     args.jobs, budget, args.timeout)
            profiles = sorted(raw.glob("*.profraw"))
            if not profiles:
                raise RuntimeError(f"training produced no profiles in {raw}")
            merged = work / f"{lto}.profdata"
            with (work / f"{lto}-merge.log").open("w", encoding="utf-8") as stream:
                subprocess.run([str(profdata), "merge", "-o", str(merged),
                                *map(str, profiles)], stdout=stream, stderr=subprocess.STDOUT, check=True)
            report["profiles"][lto] = {"path": str(merged), "sha256": file_hash(merged),
                                       "raw_files": len(profiles), "weighting": "one process per training case"}
            report["builds"][lto + "-pgo"] = build(
                lto + "-pgo", lto, f"-Cprofile-use={merged}", work, target, args, env)
            save()
        binaries = {name: Path(data["path"]) for name, data in report["builds"].items()
                    if not name.endswith("-instrumented")}
        for case in cases:
            comparison = {"case": case["name"], "role": case["role"],
                          "samples": {name: [] for name in binaries}}
            reference = None
            with tempfile.TemporaryDirectory(prefix="trace-pgo-bench-") as scratch:
                db = Path(scratch) / "analysis.db"
                # Untimed warmup also provides the strict correctness baseline.
                for name, binary in binaries.items():
                    print(f"Warmup/check {name}: {case['name']}", flush=True)
                    run_analysis(binary, case, db, work / f"{name}-{case['name']}-warmup.log",
                                 env, args.jobs, budget, args.timeout)
                    fingerprint = database_fingerprint(db)
                    if reference is None:
                        reference = fingerprint
                    else:
                        require_equal(reference, fingerprint, f"{name}/{case['name']}/warmup")
                for run in range(args.runs):
                    names = list(binaries)
                    offset = run % len(names)
                    order = names[offset:] + names[:offset]
                    for name in order:
                        print(f"Measure {name}: {case['name']} ({run + 1}/{args.runs})", flush=True)
                        sample = run_analysis(binaries[name], case, db,
                                              work / f"{name}-{case['name']}-{run}.log",
                                              env, args.jobs, budget, args.timeout, measure=True)
                        require_equal(reference, database_fingerprint(db), f"{name}/{case['name']}/{run}")
                        comparison["samples"][name].append(sample)
            comparison["identical_analysis_data"] = True
            comparison["fingerprint"] = reference
            comparison["summary"] = {name: summarize(samples)
                                     for name, samples in comparison["samples"].items()}
            baseline = comparison["summary"]["thin"]["wall_s"]["median"]
            comparison["speedup_vs_thin"] = {
                name: baseline / data["wall_s"]["median"] for name, data in comparison["summary"].items()}
            report["comparisons"].append(comparison)
            save()
            print(f"{case['name']}: " + ", ".join(
                f"{name} {data['wall_s']['median']:.3f}s ({comparison['speedup_vs_thin'][name]:.3f}x)"
                for name, data in comparison["summary"].items()), flush=True)
        if source_identity(exclude=work) != source:
            raise RuntimeError("source checkout changed during experiment; results are invalid")
        if cargo_settings(os.environ.copy(), ROOT) != settings:
            raise RuntimeError("Cargo configuration changed during experiment; results are invalid")
        for corpus in train + holdouts:
            verify_checkout(corpus)
        report["geomean_speedup_vs_thin"] = {
            role: {name: math.exp(statistics.mean(math.log(c["speedup_vs_thin"][name])
                                                for c in report["comparisons"] if c["role"] == role))
                   for name in binaries}
            for role in ("training", "holdout") if any(c["role"] == role for c in cases)}
        report["status"] = "passed"
        save()
        print(f"Passed: identical analysis data. Report: {report_path}", flush=True)
    except (Exception, KeyboardInterrupt) as exc:
        report["status"] = "failed"
        report["error"] = str(exc) or type(exc).__name__
        save()
        raise


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, subprocess.SubprocessError, OSError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        sys.exit(1)
