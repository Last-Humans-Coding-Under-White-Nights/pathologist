"""Regressions for strict PGO equivalence and measurement parsing.

Run: python3 scripts/test_build_pgo.py
"""

from contextlib import closing
from contextlib import redirect_stdout
import io
from pathlib import Path
import json
import os
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from build_pgo import (
    build, cargo_settings, checkout, conflicting_codegen_flags, database_fingerprint, main,
    parse_measurement, paths_overlap,
    require_equal, resolve_path, run_analysis, source_identity, summarize, verify_checkout,
)


class DatabaseEquivalence(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name).resolve()

    def database(self, name, rows, timestamp="today", version="baseline", extra=""):
        path = self.root / name
        with closing(sqlite3.connect(path)) as conn:
            conn.executescript("""
                CREATE TABLE analysis_run(id INTEGER, created_at TEXT, trace_version TEXT,
                                          options_json TEXT);
                CREATE TABLE facts(id INTEGER, value);
            """ + extra)
            conn.execute("INSERT INTO analysis_run VALUES(1, ?, ?, ?)",
                         (timestamp, version, '{"solver_partial":false,"solver_pops":100}'))
            conn.executemany("INSERT INTO facts VALUES(?, ?)", rows)
            conn.commit()
        return path

    def test_sorting_and_metadata_exclusion(self):
        old = self.database("old.db", [(1, "x"), (2, None), (3, b"bytes")])
        new = self.database("new.db", [(3, b"bytes"), (2, None), (1, "x")],
                            timestamp="tomorrow", version="pgo")
        require_equal(database_fingerprint(old), database_fingerprint(new), "candidate")

    def test_changed_facts_fail(self):
        old = self.database("old.db", [(1, "same"), (2, "before")])
        new = self.database("new.db", [(1, "same"), (2, "after")])
        with self.assertRaisesRegex(RuntimeError, "facts"):
            require_equal(database_fingerprint(old), database_fingerprint(new), "candidate")

    def test_duplicates_and_value_types_are_preserved(self):
        one = self.database("one.db", [(1, "value")])
        two = self.database("two.db", [(1, "value"), (1, "value")])
        blob = self.database("blob.db", [(1, b"value")])
        self.assertNotEqual(database_fingerprint(one), database_fingerprint(two))
        self.assertNotEqual(database_fingerprint(one), database_fingerprint(blob))

    def test_solver_metadata_is_checked(self):
        old = self.database("old.db", [])
        new = self.database("new.db", [])
        with closing(sqlite3.connect(new)) as conn:
            conn.execute("UPDATE analysis_run SET options_json=?", ('{"solver_partial":true}',))
            conn.commit()
        with self.assertRaisesRegex(RuntimeError, "analysis_run"):
            require_equal(database_fingerprint(old), database_fingerprint(new), "candidate")

    def test_schema_and_empty_tables_are_checked(self):
        old = self.database("old.db", [])
        new = self.database("new.db", [], extra='CREATE TABLE "empty"(id);')
        with self.assertRaisesRegex(RuntimeError, "empty"):
            require_equal(database_fingerprint(old), database_fingerprint(new), "candidate")
        with closing(sqlite3.connect(old)) as conn:
            conn.execute("ALTER TABLE facts ADD COLUMN other")
            conn.commit()
        self.assertNotEqual(database_fingerprint(old)["facts"], database_fingerprint(new)["facts"])

    def test_quoted_identifiers(self):
        path = self.database("quote.db", [], extra='CREATE TABLE "a""b"("x""y");')
        self.assertIn('a"b', database_fingerprint(path))

    def test_missing_database_is_not_created(self):
        path = self.root / "missing.db"
        with self.assertRaises(sqlite3.OperationalError):
            database_fingerprint(path)
        self.assertFalse(path.exists())

    def test_fingerprint_survives_report_serialization(self):
        fingerprint = database_fingerprint(self.database("roundtrip.db", [(1, b"data")]))
        require_equal(fingerprint, json.loads(json.dumps(fingerprint)), "saved report")

    def test_relative_paths_and_strings(self):
        path = self.database("paths.db", [(1, "value")])
        reference = database_fingerprint(path)
        relative = os.path.relpath(path)
        for value in (str(path), Path(relative), relative):
            with self.subTest(path=value):
                require_equal(reference, database_fingerprint(value), "path spelling")

    def test_missing_relative_path_is_not_created(self):
        path = self.root / "missing-relative.db"
        with self.assertRaises(sqlite3.OperationalError):
            database_fingerprint(os.path.relpath(path))
        self.assertFalse(path.exists())


class Measurements(unittest.TestCase):
    phases = "index: 1.2s (files)\nanalyze: 0.3s (edges)\nexport: 0.4s\n"

    def test_linux(self):
        sample = parse_measurement(self.phases +
                                   "TRACE_PGO_RSS_KIB=1024 TRACE_PGO_USER_S=1.50 TRACE_PGO_SYSTEM_S=0.20", 2.1)
        self.assertEqual(sample, {"wall_s": 2.1, "peak_rss_bytes": 1048576,
                                  "user_s": 1.5, "system_s": 0.2,
                                  "index_s": 1.2, "analyze_s": 0.3, "export_s": 0.4})

    def test_macos_rss_is_already_bytes(self):
        sample = parse_measurement(self.phases +
                                   "        2.00 real  1.50 user  0.20 sys\n"
                                   "  1048576  maximum resident set size\n", 2.1)
        self.assertEqual(sample["peak_rss_bytes"], 1048576)
        self.assertEqual(sample["user_s"], 1.5)
        self.assertEqual(sample["wall_s"], 2.0)

    def test_macos_comma_decimals(self):
        sample = parse_measurement(self.phases +
                                   "        2,00 real  1,50 user  0,20 sys\n"
                                   "  1048576  maximum resident set size\n", 2.1)
        self.assertEqual(sample["wall_s"], 2.0)
        self.assertEqual(sample["user_s"], 1.5)
        self.assertEqual(sample["system_s"], 0.2)
        self.assertEqual(sample["peak_rss_bytes"], 1048576)

    def test_linux_comma_decimals_and_phases(self):
        sample = parse_measurement(self.phases.replace(".", ",") +
                                   "TRACE_PGO_RSS_KIB=1024 TRACE_PGO_USER_S=1,50 "
                                   "TRACE_PGO_SYSTEM_S=0,20 TRACE_PGO_WALL_S=2,02", 2.1)
        self.assertEqual(sample["wall_s"], 2.02)
        self.assertEqual(sample["user_s"], 1.5)
        self.assertEqual(sample["system_s"], 0.2)
        self.assertEqual(sample["index_s"], 1.2)

    def test_zero_comma_wall_is_too_short_rather_than_missing(self):
        with self.assertRaisesRegex(RuntimeError, "larger workload"):
            parse_measurement(self.phases + "0,00 real 0,00 user 0,00 sys\n"
                              "1048576 maximum resident set size\n", 1)

    def test_wall_uses_time_output_without_python_wait_delay(self):
        sample = parse_measurement(self.phases +
                                   "TRACE_PGO_RSS_KIB=1024 TRACE_PGO_USER_S=1.50 "
                                   "TRACE_PGO_SYSTEM_S=0.20 TRACE_PGO_WALL_S=2.02", 2.1)
        self.assertEqual(sample["wall_s"], 2.02)

    def test_missing_measurements_fail(self):
        with self.assertRaisesRegex(RuntimeError, "time measurement"):
            parse_measurement(self.phases, 1)
        with self.assertRaisesRegex(RuntimeError, "phase measurements"):
            parse_measurement("TRACE_PGO_RSS_KIB=1 TRACE_PGO_USER_S=1 TRACE_PGO_SYSTEM_S=1", 1)
        with self.assertRaisesRegex(RuntimeError, "time measurement"):
            parse_measurement(self.phases + "TRACE_PGO_RSS_KIB=1", 1)

    def test_sub_resolution_workload_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "larger workload"):
            parse_measurement(self.phases + "TRACE_PGO_RSS_KIB=1 TRACE_PGO_USER_S=0 "
                              "TRACE_PGO_SYSTEM_S=0 TRACE_PGO_WALL_S=0.00", 0.04)

    def test_summary_reports_spread(self):
        self.assertEqual(summarize([{"wall_s": 1}, {"wall_s": 9}, {"wall_s": 2}]),
                         {"wall_s": {"median": 2, "min": 1, "max": 9}})


class SourceSnapshots(unittest.TestCase):
    def test_output_inside_checkout_is_excluded_but_source_changes_are_detected(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch).resolve()
            subprocess.run(["git", "init", "-q", str(root)], check=True, capture_output=True)
            source = root / "main.rs"
            source.write_text("fn main() {}\n")
            with patch("build_pgo.ROOT", root):
                before = source_identity()
                work = root / "custom output"
                work.mkdir()
                (work / "binary").write_bytes(b"generated")
                self.assertEqual(before, source_identity(exclude=work))
                self.assertNotEqual(before, source_identity())
                source.write_text("fn main() { panic!(); }\n")
                self.assertNotEqual(before, source_identity(exclude=work))


class CorpusSnapshots(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name).resolve()
        self.corpus = self.root / "corpus[1]"
        self.corpus.mkdir()
        self.git("init", "-q")
        (self.root / ".gitignore").write_text(
            "**/generated/\n**/build/\n**/compile_flags.txt\n", encoding="utf-8")
        (self.corpus / "main.cpp").write_text("int main() {}\n", encoding="utf-8")
        self.git("add", ".")
        self.git("-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                 "commit", "-qm", "corpus")
        self.generated = self.corpus / "generated"
        self.generated.mkdir()
        self.source = self.generated / "extra.c"
        self.source.write_text("int before;\n", encoding="utf-8")
        self.config = self.corpus / "build/compile_commands.json"
        self.config.parent.mkdir()
        self.config.write_text("[]\n", encoding="utf-8")

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root).decode().strip()

    def commit(self):
        self.git("-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                 "commit", "-qm", "update corpus")

    def test_hidden_untracked_inputs_fail_preflight_and_final_check(self):
        self.git("config", "status.showUntrackedFiles", "no")
        before = checkout(self.corpus)
        extra = self.corpus / "new.cpp"
        extra.write_text("int new_input;\n", encoding="utf-8")
        self.assertEqual(self.git("status", "--porcelain"), "")
        for check in (lambda: checkout(self.corpus), lambda: verify_checkout(before)):
            with self.assertRaisesRegex(ValueError, "corpus has local changes"):
                check()

    def test_tracked_submodules_are_rejected_but_can_be_selected_separately(self):
        seed = self.root / "dep-source"
        seed.mkdir()
        subprocess.run(["git", "init", "-q", str(seed)], check=True)
        (seed / ".gitignore").write_text("generated/\n", encoding="utf-8")
        (seed / "main.cpp").write_text("int dep;\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(seed), "add", "."], check=True)
        subprocess.run(["git", "-C", str(seed), "-c", "user.name=Test",
                        "-c", "user.email=test@example.invalid", "commit", "-qm", "dep"],
                       check=True)
        module = self.corpus / "dep"
        self.git("-c", "protocol.file.allow=always", "submodule", "add", "--quiet",
                 str(seed), str(module.relative_to(self.root)))
        self.commit()
        # Even configurations that suppress submodule changes cannot skip the
        # explicit gitlink check. Ignored generated sources leave both clean.
        self.git("config", "diff.ignoreSubmodules", "all")
        generated = module / "generated"
        generated.mkdir()
        source = generated / "extra.cpp"
        source.write_text("int before;\n", encoding="utf-8")
        self.assertEqual(self.git("status", "--porcelain", "--",
                                  str(self.corpus.relative_to(self.root))), "")
        self.assertEqual(subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=module), b"")
        with self.assertRaisesRegex(ValueError, "corpus contains tracked submodule"):
            checkout(self.corpus)
        before = checkout(module)
        source.write_text("int after;\n", encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)
        # The selected subtree has no submodule, even though the checkout does.
        verify_checkout(checkout(self.generated))

    def test_tracked_header_symlink_detects_external_target_content_changes(self):
        target = self.root / "external.h"
        target.write_text("void before() {}\n", encoding="utf-8")
        link = self.corpus / "header.h"
        link.symlink_to(target)
        self.git("add", str(link.relative_to(self.root)))
        self.commit()
        before = checkout(self.corpus)
        self.assertEqual(before["symlink_inputs"]["files"], 1)
        verify_checkout(before)
        target.write_text("void after() {}\n", encoding="utf-8")
        self.assertEqual(self.git("status", "--porcelain", "--",
                                  str(self.corpus.relative_to(self.root))), "")
        self.assertEqual(before["revision"], checkout(self.corpus)["revision"])
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)
        target.unlink()
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)

    def test_tracked_directory_symlink_is_rejected(self):
        link = self.corpus / "includes"
        link.symlink_to(self.generated, target_is_directory=True)
        self.git("add", str(link.relative_to(self.root)))
        self.commit()
        with self.assertRaisesRegex(ValueError, "cannot snapshot symlink corpus input"):
            checkout(self.corpus)

    def test_tracked_symlink_chain_detects_resolved_target_changes(self):
        first = self.root / "first.h"
        second = self.root / "second.h"
        first.write_text("int identical;\n", encoding="utf-8")
        second.write_bytes(first.read_bytes())
        intermediate = self.root / "external.h"
        intermediate.symlink_to(first)
        link = self.corpus / "header.h"
        link.symlink_to(intermediate)
        self.git("add", str(link.relative_to(self.root)))
        self.commit()
        before = checkout(self.corpus)
        intermediate.unlink()
        intermediate.symlink_to(second)
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)

    def test_ignored_sources_and_compilation_database_are_recorded_and_rechecked(self):
        self.assertEqual(self.git("status", "--porcelain"), "")
        before = checkout(self.corpus)
        self.assertEqual(before["ignored_inputs"]["files"], 2)
        verify_checkout({"name": "holdout", **before})
        for path in (self.source, self.config):
            with self.subTest(path=path):
                original = path.read_bytes()
                path.write_bytes(original + b"changed\n")
                self.assertEqual(before["revision"], checkout(self.corpus)["revision"])
                with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
                    verify_checkout(before)
                path.write_bytes(original)

    def test_adding_removing_and_renaming_ignored_inputs_invalidate_snapshot(self):
        before = checkout(self.corpus)
        extra = self.generated / "header.hpp"
        extra.write_text("struct S {};\n", encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)
        extra.unlink()
        renamed = self.source.with_name("renamed.c")
        self.source.rename(renamed)
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)
        renamed.unlink()
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)

    def test_ignored_changes_outside_selected_subtree_are_excluded(self):
        before = checkout(self.corpus)
        outside = self.root / "build"
        outside.mkdir()
        (outside / "other.c").write_text("int other;\n", encoding="utf-8")
        verify_checkout(before)
        self.assertEqual(checkout(self.root)["ignored_inputs"]["files"], 3)

    def test_entire_ignored_subtree_is_snapshotted(self):
        before = checkout(self.generated)
        self.assertEqual(before["ignored_inputs"]["files"], 1)
        self.source.write_text("int after;\n", encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)

    def test_ignored_directory_symlink_is_rejected(self):
        (self.generated / "linked").symlink_to(self.corpus, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "cannot snapshot ignored corpus input"):
            checkout(self.corpus)

    def test_ignored_nested_repository_is_rejected(self):
        nested = self.generated / "nested"
        subprocess.run(["git", "init", "-q", str(nested)], check=True)
        (nested / "hidden.c").write_text("int hidden;\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "cannot snapshot ignored corpus input"):
            checkout(self.corpus)

    def test_ignored_file_symlink_tracks_target_content_and_link_spelling(self):
        link = self.generated / "header.h"
        link.symlink_to(self.source)
        before = checkout(self.corpus)
        link.unlink()
        link.symlink_to("extra.c")
        with self.assertRaisesRegex(RuntimeError, "corpus inputs changed"):
            verify_checkout(before)


class RustFlags(unittest.TestCase):
    def test_conflicts_cover_aliases_and_codegen_argument_forms(self):
        for option in ("profile-generate=/tmp/pgo", "profile-use=/tmp/pgo",
                       "codegen-units=8", "lto", "lto=thin"):
            for spelling in (option, option.replace("-", "_")):
                for flags in (["-C" + spelling], ["-C", spelling], ["-C=" + spelling],
                              ["--codegen", spelling], ["--codegen=" + spelling]):
                    with self.subTest(flags=flags):
                        self.assertTrue(conflicting_codegen_flags(flags))

    def test_unrelated_flags_and_values_are_preserved(self):
        flags = ["-Ctarget_cpu=native", "-C", "link-arg=/tmp/profile_generate.o",
                 "--cfg", 'label="codegen_units=8"']
        original = flags.copy()
        self.assertFalse(conflicting_codegen_flags(flags))
        self.assertEqual(flags, original)


@unittest.skipUnless(sys.version_info >= (3, 11), "Cargo config tests require Python's TOML reader")
class CargoSettings(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name).resolve()
        self.project = self.root / "project"
        self.project.mkdir()
        self.env = {"HOME": str(self.root / "home"), "CARGO_HOME": str(self.root / "cargo-home")}

    def config(self, directory, body, name="config.toml"):
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / name
        path.write_text(body, encoding="utf-8")
        return path

    def test_build_and_target_configured_flags_are_rejected(self):
        for table in ("build", "target.x86_64-unknown-linux-gnu", 'target.\'cfg(unix)\''):
            with self.subTest(table=table):
                path = self.config(self.project / ".cargo",
                                   f'[{table}]\nrustflags = ["--cfg", "pgo_config_probe"]\n')
                with self.assertRaisesRegex(ValueError, "unsupported Cargo-configured Rust flags") as error:
                    cargo_settings(self.env, self.project)
                self.assertIn(str(path), str(error.exception))

    def test_cargo_flag_environment_aliases_are_rejected(self):
        for key in ("CARGO_BUILD_RUSTFLAGS", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"):
            with self.subTest(key=key):
                with self.assertRaisesRegex(ValueError, key):
                    cargo_settings({**self.env, key: "--cfg pgo_config_probe"}, self.project)

    def test_explicit_flags_preserve_cargo_precedence_and_literal_quotes(self):
        self.config(self.project / ".cargo", '[build]\nrustflags = ["--cfg", "configured"]\n')
        env = {**self.env, "CARGO_BUILD_RUSTFLAGS": "--cfg ignored",
               "RUSTFLAGS": '--cfg probe="enabled" -Ctarget-cpu=native'}
        self.assertEqual(cargo_settings(env, self.project)["rustflags"],
                         ["--cfg", 'probe="enabled"', "-Ctarget-cpu=native"])
        encoded = ["--cfg", 'probe="has spaces"']
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(encoded)
        self.assertEqual(cargo_settings(env, self.project)["rustflags"], encoded)
        env["CARGO_ENCODED_RUSTFLAGS"] = ""
        self.assertEqual(cargo_settings(env, self.project)["rustflags"], [])

    def test_compiler_uses_hierarchy_home_legacy_and_environment_precedence(self):
        self.config(Path(self.env["CARGO_HOME"]), '[build]\nrustc = "home-rustc"\n')
        self.config(self.root / ".cargo", '[build]\nrustc = "parent-rustc"\n')
        self.assertEqual(cargo_settings(self.env, self.project)["compiler"], "parent-rustc")
        self.config(self.project / ".cargo", '[build]\nrustc = "toml-rustc"\n')
        self.config(self.project / ".cargo", '[build]\nrustc = "./bin/legacy-rustc"\n', "config")
        self.assertEqual(cargo_settings(self.env, self.project)["compiler"],
                         str(self.project / "bin/legacy-rustc"))
        env = {**self.env, "CARGO_BUILD_RUSTC": "build-env-rustc"}
        self.assertEqual(cargo_settings(env, self.project)["compiler"], "build-env-rustc")
        env["RUSTC"] = "explicit-rustc"
        self.assertEqual(cargo_settings(env, self.project)["compiler"], "explicit-rustc")

    def test_flags_in_home_and_ancestor_configs_are_not_silently_dropped(self):
        for directory in (Path(self.env["CARGO_HOME"]), self.root / ".cargo"):
            with self.subTest(directory=directory):
                config = self.config(directory, '[build]\nrustflags = ["-Ctarget-cpu=native"]\n')
                with self.assertRaisesRegex(ValueError, "unsupported Cargo-configured Rust flags"):
                    cargo_settings(self.env, self.project)
                config.unlink()

    def test_config_includes_are_explicitly_rejected(self):
        self.config(self.project / ".cargo", 'include = ["extra.toml"]\n')
        with self.assertRaisesRegex(ValueError, "Cargo config includes are unsupported"):
            cargo_settings(self.env, self.project)

    def test_configuration_changes_alter_recorded_identity(self):
        config = self.config(self.project / ".cargo", '[net]\noffline = true\n')
        before = cargo_settings(self.env, self.project)
        config.write_text('[net]\noffline = false\n', encoding="utf-8")
        self.assertNotEqual(before, cargo_settings(self.env, self.project))

    @unittest.skipUnless(shutil.which("cargo") and shutil.which("rustc"), "Cargo release probe")
    def test_real_build_overrides_named_and_wildcard_package_codegen_units(self):
        sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
        cargo = str(sysroot / "bin/cargo")
        compiler = self.project / "compiler-wrapper"
        log = self.root / "compiler.jsonl"
        compiler.write_text('#!/usr/bin/env python3\nimport json, os, sys\n'
                            'with open(os.environ["COMPILER_PROBE_LOG"], "a") as out:\n'
                            '    out.write(json.dumps(sys.argv[1:]) + "\\n")\n'
                            'os.execv(os.environ["COMPILER_PROBE_REAL"], '
                            '[os.environ["COMPILER_PROBE_REAL"], *sys.argv[1:]])\n', encoding="utf-8")
        compiler.chmod(0o700)
        (self.project / "src").mkdir()
        (self.project / "src/main.rs").write_text('fn main() { println!("{}", unit_dep::answer()); }\n')
        (self.project / "Cargo.toml").write_text(
            '[package]\nname="trace-cli"\nversion="0.1.0"\nedition="2021"\n'
            '[[bin]]\nname="trace"\npath="src/main.rs"\n'
            '[dependencies]\nunit-dep={path="dep"}\n', encoding="utf-8")
        dep = self.project / "dep"
        (dep / "src").mkdir(parents=True)
        (dep / "Cargo.toml").write_text('[package]\nname="unit-dep"\nversion="0.1.0"\n')
        (dep / "src/lib.rs").write_text('pub fn answer() -> u32 { 42 }\n')
        self.config(self.project / ".cargo", '[profile.release.package.trace-cli]\ncodegen-units=8\n'
                    '[profile.release.package."*"]\ncodegen-units=8\n')
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CARGO_", "RUST"))}
        env.update({**self.env, "RUSTC": str(compiler), "COMPILER_PROBE_LOG": str(log),
                    "COMPILER_PROBE_REAL": str(sysroot / "bin/rustc"), "CARGO_ENCODED_RUSTFLAGS": "",
                    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1"})
        subprocess.run([cargo, "generate-lockfile", "--offline"], cwd=self.project, env=env,
                       capture_output=True, check=True)
        target = re.search(r"^host: (.+)$", subprocess.check_output(
            [str(sysroot / "bin/rustc"), "-Vv"], text=True), re.MULTILINE)[1]
        work = self.root / "artifacts"
        work.mkdir()

        def effective_units():
            result = {}
            for line in log.read_text(encoding="utf-8").splitlines():
                args = json.loads(line)
                if "--crate-name" not in args:
                    continue
                name = args[args.index("--crate-name") + 1]
                if name not in ("trace", "unit_dep"):
                    continue
                units = []
                for index, arg in enumerate(args):
                    option = args[index + 1] if arg == "-C" else arg.removeprefix("-C")
                    if option.startswith("codegen-units="):
                        units.append(int(option.split("=", 1)[1]))
                result[name] = units[-1]
            return result

        subprocess.run([cargo, "build", "--offline", "--release", "--target", target,
                        "--target-dir", str(work / "ordinary")], cwd=self.project, env=env,
                       capture_output=True, check=True)
        self.assertEqual(effective_units(), {"trace": 8, "unit_dep": 8})
        # Use the actual workflow build, including its explicit native target.
        with patch("build_pgo.ROOT", self.project), redirect_stdout(io.StringIO()):
            for label, profile in (("thin", None), ("instrumented", f"-Cprofile-generate={work / 'raw'}")):
                log.write_text("", encoding="utf-8")
                # Run the installed Cargo directly despite isolated rustup state.
                original_run = subprocess.run
                with patch("build_pgo.subprocess.run",
                           side_effect=lambda command, **kwargs: original_run([cargo, *command[1:]], **kwargs)):
                    build(label, "thin", profile, work, target,
                          SimpleNamespace(build_jobs=1, features=""), env)
                self.assertEqual(effective_units(), {"trace": 1, "unit_dep": 1})

    @unittest.skipUnless(shutil.which("cargo") and shutil.which("rustc"), "Cargo compiler probe")
    def test_real_cargo_uses_the_compiler_selected_for_preflight(self):
        # Use installed binaries directly; the isolated HOME/CARGO_HOME must
        # not make rustup proxies look for a second toolchain installation.
        sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
        actual = str(sysroot / "bin/rustc")
        cargo = str(sysroot / "bin/cargo") if (sysroot / "bin/cargo").is_file() else "cargo"
        compiler = self.project / "compiler-wrapper"
        log = self.root / "compiler.log"
        compiler.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$COMPILER_PROBE_LOG"\n'
                            'exec "$COMPILER_PROBE_REAL" "$@"\n', encoding="utf-8")
        compiler.chmod(0o700)
        (self.project / "Cargo.toml").write_text(
            '[package]\nname = "pgo-config-probe"\nversion = "0.1.0"\nedition = "2021"\n',
            encoding="utf-8")
        (self.project / "src").mkdir()
        (self.project / "src/main.rs").write_text(
            'fn main() { println!("{}", cfg!(pgo_config_probe)); }\n', encoding="utf-8")
        clean_env = {key: value for key, value in os.environ.items()
                     if not key.startswith(("CARGO_", "RUST"))}
        env = {**clean_env, **self.env, "COMPILER_PROBE_LOG": str(log),
               "COMPILER_PROBE_REAL": actual}
        # Exercise the environment alias and the config-relative executable.
        for configured in (False, True):
            with self.subTest(configured=configured):
                if configured:
                    self.config(self.project / ".cargo", '[build]\nrustc = "./compiler-wrapper"\n')
                    env.pop("CARGO_BUILD_RUSTC")
                else:
                    env["CARGO_BUILD_RUSTC"] = str(compiler)
                settings = cargo_settings(env, self.project)
                self.assertEqual(settings["compiler"], str(compiler))
                version = subprocess.check_output([settings["compiler"], "-Vv"],
                                                  cwd=self.project, env=env)
                self.assertIn(b"LLVM version:", version)
                log.unlink()
                build_env = {**env, "RUSTC": settings["compiler"],
                             "CARGO_BUILD_RUSTC": str(self.project / "missing-compiler"),
                             "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1fpgo_config_probe"}
                subprocess.run([cargo, "build", "--offline", "--target-dir",
                                str(self.root / f"target-{configured}")], cwd=self.project,
                               env=build_env, capture_output=True, check=True)
                self.assertIn("--crate-name", log.read_text(encoding="utf-8"))
                binary = self.root / f"target-{configured}/debug/pgo-config-probe"
                self.assertEqual(subprocess.check_output([str(binary)]).strip(), b"true")

        # Reproduce the configured-flag case with Cargo itself. The workflow
        # rejects it before replacing those flags; an explicit override keeps cfg.
        self.config(self.project / ".cargo", '[build]\nrustc = "./compiler-wrapper"\n'
                    'rustflags = ["--cfg", "pgo_config_probe"]\n')
        ordinary = subprocess.check_output(
            [cargo, "run", "--offline", "--quiet", "--target-dir", str(self.root / "ordinary")],
            cwd=self.project, env=env, stderr=subprocess.PIPE)
        self.assertEqual(ordinary.strip(), b"true")
        with self.assertRaisesRegex(ValueError, "unsupported Cargo-configured Rust flags"):
            cargo_settings(env, self.project)
        settings = cargo_settings({**env, "RUSTFLAGS": "--cfg pgo_config_probe"}, self.project)
        self.assertEqual(settings["rustflags"], ["--cfg", "pgo_config_probe"])


@unittest.skipUnless(sys.platform in ("linux", "darwin"), "native process groups and /usr/bin/time")
class AnalysisProcesses(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name).resolve()
        self.db = self.root / "analysis.db"
        self.log = self.root / "analysis.log"
        self.binary = self.root / "analyzer"
        self.case = {"path": str(self.root), "args": []}

    def analyzer(self, body):
        self.binary.write_text("#!/bin/sh\n" + body, encoding="utf-8")
        self.binary.chmod(0o700)

    def run_analysis(self, env=None, measure=False):
        return run_analysis(self.binary, self.case, self.db, self.log,
                            env if env is not None else os.environ.copy(),
                            1, 800000, 10, measure=measure)

    def test_noop_candidate_cannot_reuse_previous_database(self):
        self.analyzer("exit 0\n")
        for measure in (False, True):
            with self.subTest(measure=measure):
                with closing(sqlite3.connect(self.db)) as conn:
                    conn.execute("CREATE TABLE stale(value)")
                    conn.commit()
                with self.assertRaisesRegex(RuntimeError, "no fresh database"):
                    self.run_analysis(measure=measure)
                self.assertFalse(self.db.exists())

    def test_runner_resolves_relative_output_paths_before_deleting_stale_db(self):
        self.analyzer("exit 0\n")
        self.db.write_bytes(b"stale")
        with self.assertRaisesRegex(RuntimeError, "no fresh database"):
            run_analysis(self.binary, self.case, os.path.relpath(self.db), os.path.relpath(self.log),
                         os.environ.copy(), 1, 800000, 10)
        self.assertFalse(self.db.exists())

    def test_stale_output_symlink_is_removed_without_deleting_its_target(self):
        self.analyzer("exit 0\n")
        target = self.root / "previous.db"
        target.write_bytes(b"previous database")
        self.db.symlink_to(target)
        with self.assertRaisesRegex(RuntimeError, "no fresh database"):
            self.run_analysis()
        self.assertFalse(self.db.is_symlink())
        self.assertEqual(target.read_bytes(), b"previous database")

    def test_native_timer_forces_c_locale_and_replaces_invalid_log_bytes(self):
        self.analyzer(r'''
while [ "$#" -gt 0 ]; do
    if [ "$1" = "-o" ]; then shift; db="$1"; fi
    shift
done
: > "$db"
printf '%s\n' "LC_ALL=$LC_ALL"
printf '\377noise\nindex: 1.2s\nanalyze: 0.3s\nexport: 0.4s\n'
sleep 0.1
''')
        env = os.environ.copy()
        env["LC_ALL"] = "ru_RU.UTF-8"
        sample = self.run_analysis(env, measure=True)
        self.assertGreater(sample["wall_s"], 0)
        self.assertEqual(sample["index_s"], 1.2)
        self.assertEqual(env["LC_ALL"], "ru_RU.UTF-8")
        log = self.log.read_text(encoding="utf-8", errors="replace")
        self.assertIn("LC_ALL=C", log)
        self.assertIn("\ufffdnoise", log)

    def test_process_group_exit_race_preserves_timeout_and_interrupt(self):
        for exception, expected in (
            (subprocess.TimeoutExpired("analyzer", 10), RuntimeError),
            (KeyboardInterrupt(), KeyboardInterrupt),
        ):
            with self.subTest(exception=type(exception).__name__):
                with patch("build_pgo.subprocess.Popen") as popen, \
                        patch("build_pgo.os.killpg", side_effect=ProcessLookupError):
                    proc = popen.return_value.__enter__.return_value
                    proc.wait.side_effect = [exception, 0]
                    with self.assertRaises(expected) as caught:
                        self.run_analysis()
                    if expected is RuntimeError:
                        self.assertIn("timed out", str(caught.exception))
                    self.assertEqual(proc.wait.call_count, 2)


@unittest.skipUnless(sys.platform in ("linux", "darwin"), "native experiment preflight")
class PathsAndPreflight(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name).resolve()
        self.expected = self.root / "expected.json"

    def configure(self, corpus):
        self.expected.write_text(json.dumps({"budget_pops": 800000, "corpora": {
            "hdf": {"dir": str(corpus), "rev": "revision"}}}), encoding="utf-8")

    def preflight(self, flags):
        stderr = io.StringIO()

        def fake_checkout(path, revision=None):
            return {"path": str(resolve_path(path)), "revision": revision or "revision"}

        def fake_output(command, **kwargs):
            return ("LLVM version 22.1.2" if command[-1] == "--version"
                    else "host: native\nLLVM version: 22.1.2")

        with patch("build_pgo.ROOT", self.root), \
                patch("build_pgo.checkout", side_effect=fake_checkout) as check, \
                patch("build_pgo.output", side_effect=fake_output), \
                patch("build_pgo.build") as build, \
                patch("sys.argv", ["build_pgo.py", "--expected", str(self.expected),
                                   "--train", "hdf", *flags]), \
                patch("sys.stderr", stderr):
            with self.assertRaises(SystemExit) as caught:
                main()
            self.assertEqual(caught.exception.code, 2)
            build.assert_not_called()
        return stderr.getvalue(), check.call_args_list

    def test_checkout_expands_quoted_tilde(self):
        corpus = self.root / "corpus"
        corpus.mkdir()
        with patch.dict(os.environ, {"HOME": str(self.root)}), \
                patch("build_pgo.git_output", side_effect=[str(self.root), "revision", ""]), \
                patch("build_pgo.subprocess.check_output", return_value=b""):
            self.assertEqual(checkout("~/corpus")["path"], str(corpus))

    def test_rust_flag_aliases_fail_preflight_for_both_environment_encodings(self):
        self.configure(self.root / "corpus")
        profdata = self.root / "llvm-profdata"
        profdata.touch()
        clean_env = {key: value for key, value in os.environ.items()
                     if key not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
        for flags in (["-Cprofile_generate=/tmp/raw"], ["-Ccodegen_units=8"],
                      ["-C", "profile_use=/tmp/merged"], ["--codegen=codegen_units=8"]):
            for variable in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
                with self.subTest(flags=flags, variable=variable):
                    value = shlex.join(flags) if variable == "RUSTFLAGS" else "\x1f".join(flags)
                    with patch.dict(os.environ, {**clean_env, variable: value}, clear=True):
                        error, _ = self.preflight(["--llvm-profdata", str(profdata)])
                    self.assertIn("remove PGO/LTO/codegen-units overrides", error)
                    self.assertFalse((self.root / "target").exists())

    @unittest.skipUnless(sys.version_info >= (3, 11), "Cargo config needs Python's TOML reader")
    def test_cargo_configured_flags_fail_before_creating_artifacts(self):
        self.configure(self.root / "corpus")
        config = self.root / ".cargo/config.toml"
        config.parent.mkdir()
        config.write_text('[build]\nrustflags = ["--cfg", "pgo_config_probe"]\n', encoding="utf-8")
        env = {key: value for key, value in os.environ.items()
               if key not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
        with patch.dict(os.environ, env, clear=True):
            error, _ = self.preflight([])
        self.assertIn("unsupported Cargo-configured Rust flags", error)
        self.assertIn(str(config), error)
        self.assertFalse((self.root / "target").exists())

    def test_cli_paths_expand_tilde_and_bad_override_has_specific_error(self):
        corpus = self.root / "corpus"
        self.configure(corpus)
        with patch.dict(os.environ, {"HOME": str(self.root)}):
            error, calls = self.preflight([
                "--expected", "~/expected.json", "--holdout", "~/held",
                "--outdir", "~/output", "--llvm-profdata", "~/missing-profdata",
            ])
        self.assertIn(str(self.root / "missing-profdata"), error)
        self.assertIn("--llvm-profdata is not an existing file", error)
        self.assertNotIn("rustup component add", error)
        self.assertEqual(calls[1].args[0], self.root / "held")
        self.assertFalse((self.root / "output").exists())

    def test_explicit_artifacts_cannot_contain_or_be_inside_corpora(self):
        for corpus, destination in (
            (self.root / "corpus", self.root / "corpus"),
            (self.root / "corpus", self.root / "corpus/output"),
            (self.root / "output/corpus", self.root / "output"),
        ):
            with self.subTest(corpus=corpus, destination=destination):
                self.configure(corpus)
                error, _ = self.preflight(["--outdir", str(destination)])
                self.assertIn("must not contain or be inside", error)
                self.assertFalse(destination.exists())

    def test_default_artifact_parent_is_checked_in_both_directions(self):
        for corpus in (self.root, self.root / "target/pgo", self.root / "target/pgo/corpus"):
            with self.subTest(corpus=corpus):
                self.configure(corpus)
                error, _ = self.preflight([])
                self.assertIn("must not contain or be inside", error)
                self.assertFalse((self.root / "target").exists())

    def test_containment_checks_resolve_symlinks(self):
        corpus = self.root / "corpus"
        corpus.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(corpus, target_is_directory=True)
        self.assertTrue(paths_overlap(alias / "output", corpus))
        self.assertTrue(paths_overlap(corpus, alias / "output"))


if __name__ == "__main__":
    unittest.main()
