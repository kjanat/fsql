"""Contract tests use installed tools and fail closed on divergent output."""

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from benchmarks import run as bench


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="fsql bench ' ")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        bench.fixture(self.root, 30)
        (self.root / "line\nbreak.rs").write_text("newline")
        (self.root / "\n").write_text("single newline")
        (self.root / "carriage\rreturn.rs").write_text("carriage")
        (self.root / ".ignore").write_text("*.rs\n")
        (self.root / "uppercase.RS").write_text("case sensitive")
        (self.root / "edge").write_bytes(b"x" * 100)
        (self.root / "above").write_bytes(b"x" * 101)
        (self.root / "edge-link").symlink_to("above")

    def test_adapters_match_independent_oracle(self):
        rows = bench.inventory(self.root)
        binary = bench.REPO / "target/release/fsql"
        self.assertTrue(
            binary.is_file(), "build release fsql before running integration checks"
        )
        for case in bench.CASES:
            reference = bench.expected(rows, case, 100)
            for legacy in [False, True]:
                with self.subTest(case=case, legacy=legacy):
                    command = bench.fsql_command(
                        "fsql", str(binary), case, self.root, 100, legacy
                    )
                    bench.verify(command, self.root, 10, reference)
            for tool in ["find", "fd", "ls", "bfs", "fselect"]:
                if not (tool_binary := shutil.which(tool)):
                    continue
                command = bench.tool_command(tool, tool_binary, case, self.root, 100, 1)
                if command:
                    with self.subTest(case=case, tool=tool):
                        bench.verify(command, self.root, 10, reference)
        self.assertNotIn("edge", bench.expected(rows, "size", 100))
        self.assertIn("above", bench.expected(rows, "size", 100))
        self.assertNotIn("edge-link", bench.expected(rows, "size", 100))

    def test_duplicates_and_stderr_cannot_pass(self):
        command = bench.Command(
            "fake", "paths", [sys.executable, "-c", "print('a\\0a\\0', end='')"], "nul"
        )
        with self.assertRaisesRegex(ValueError, "result mismatch"):
            bench.verify(command, self.root, 10, bench.Counter(["a"]))
        command.argv = [
            sys.executable,
            "-c",
            "import sys; print('warning', file=sys.stderr)",
        ]
        with self.assertRaisesRegex(ValueError, "warning"):
            bench.run(command, self.root, 10, False)

    def test_json_paths_preserve_special_characters(self):
        command = bench.Command("fsql", "paths", [], "json")
        path = self.root / "quote' line\n.rs"
        self.assertEqual(
            bench.decode(
                command, (json.dumps({"path": str(path)}) + "\n").encode(), self.root
            ),
            [path.name],
        )
        with self.assertRaises(ValueError):
            bench.decode(command, b'{"path":"/outside"}\n', self.root)

    def test_snapshot_detects_same_size_changes(self):
        before = bench.inventory(self.root)
        (self.root / "above").write_bytes(b"y" * 101)
        self.assertNotEqual(before, bench.inventory(self.root))

    def test_gahag_framing_does_not_strip_filename_whitespace(self):
        command = bench.Command(
            "gahag", "paths", [], "gahag", b"select name from .\nquit\n"
        )
        output = b"fsql> select name from .\n spaced \n\nfsql> "
        self.assertEqual(bench.decode(command, output, self.root), [" spaced "])

    def test_gahag_framing_requires_query_input(self):
        for stdin in (None, b""):
            with self.subTest(stdin=stdin):
                command = bench.Command("gahag", "paths", [], "gahag", stdin)
                with self.assertRaisesRegex(
                    ValueError, "missing gahag REPL query input"
                ):
                    bench.decode(command, b"fsql> ", self.root)

    def test_recovery_name_grammar_matches_walker(self):
        for name in [
            ".fsql-notes",
            ".fsql-cache",
            ".fsql-a-b-no",
            ".fsql-g-1-2",
            ".fsql--1-2",
            ".fsql-a-b-1-extra",
            ".fsql-a-b-١",
        ]:
            (self.root / name).mkdir()
            (self.root / name / "ordinary").write_text("keep")
        rows = bench.inventory(self.root)
        self.assertIn(".fsql-notes/ordinary", rows)
        self.assertIn(".fsql-cache/ordinary", rows)
        command = bench.fsql_command(
            "fsql",
            str(bench.REPO / "target/release/fsql"),
            "paths",
            self.root,
            100,
            False,
        )
        bench.verify(command, self.root, 10, bench.expected(rows, "paths", 100))
        for name in [".fsql-a-b-1", ".fsql-ABC-012-00"]:
            for parent in [self.root, self.root / ".fsql-notes"]:
                reserved = parent / name
                reserved.mkdir()
                try:
                    with self.assertRaisesRegex(ValueError, "private"):
                        bench.inventory(self.root)
                finally:
                    reserved.rmdir()

    def test_changed_harness_preserves_previous_report(self):
        harness = self.root / "harness.py"
        harness.write_text("original")
        report = self.root / "report.json"
        report.write_text('{"previous": true}')
        argv = [
            "bench",
            "--files",
            "1",
            "--tools",
            "--case",
            "paths",
            "--runs",
            "1",
            "--warmups",
            "0",
            "--output",
            str(report),
        ]

        def change_harness(*args, **kwargs):
            harness.write_text("replacement")
            return 1.0, b""

        with (
            patch.object(sys, "argv", argv),
            patch.object(bench, "__file__", str(harness)),
            patch.object(bench, "verify", return_value=[]),
            patch.object(bench, "run", side_effect=change_harness),
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(ValueError, "harness changed"),
        ):
            bench.main()
        self.assertEqual(json.loads(report.read_text()), {"previous": True})

    def test_complete_run_writes_verified_report(self):
        import subprocess

        report = self.root / "report.json"
        command = [
            sys.executable,
            str(bench.REPO / "benchmarks/run.py"),
            "--files",
            "20",
            "--runs",
            "1",
            "--warmups",
            "0",
            "--tools",
            "--adapter",
            str(bench.REPO / "benchmarks/adapters/find-regex.toml"),
            "--output",
            str(report),
        ]
        result = subprocess.run(command, capture_output=True, timeout=30, check=False)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        data = json.loads(report.read_text())
        self.assertEqual(data["schema_version"], 1)
        self.assertEqual(data["harness_sha256"], bench.digest(bench.__file__))
        self.assertEqual(len(data["results"]), 6)
        self.assertTrue(all(len(row["samples_ms"]) == 1 for row in data["results"]))
        self.assertTrue(
            all(len(binary["sha256"]) == 64 for binary in data["binaries"].values())
        )

    def test_relative_path_tools_survive_changed_working_directory(self):
        tool = shutil.which("find")
        if tool is None:
            self.fail("find must be installed for the relative-PATH regression test")
        (self.root / "bin").mkdir()
        (self.root / "bin/find").symlink_to(Path(tool).absolute())
        report = self.root / "relative-path-report.json"
        result = subprocess.run(
            [
                sys.executable,
                str(bench.REPO / "benchmarks/run.py"),
                "--tools",
                "find",
                "--case",
                "paths",
                "--files",
                "20",
                "--runs",
                "1",
                "--warmups",
                "0",
                "--output",
                str(report),
            ],
            cwd=self.root,
            env={**os.environ, "PATH": "bin"},
            capture_output=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        rows = json.loads(report.read_text())["results"]
        command = next(row for row in rows if row["tool"] == "find")
        self.assertEqual(command["argv"][0], str(self.root / "bin/find"))

    def test_report_replacement_preserves_previous_report_on_failure(self):
        report = self.root / "report.json"
        report.write_text('{"previous": true}\n')
        before = set(self.root.iterdir())
        for target in ["os.fsync", "os.replace"]:
            with (
                self.subTest(target=target),
                patch.object(
                    bench.os, target.split(".")[1], side_effect=OSError("disk failure")
                ),
                self.assertRaises(OSError),
            ):
                bench.publish_report(report, {"new": True})
            self.assertEqual(json.loads(report.read_text()), {"previous": True})
            self.assertEqual(set(self.root.iterdir()), before)
        bench.publish_report(report, {"new": True})
        self.assertEqual(json.loads(report.read_text()), {"new": True})
        self.assertEqual(set(self.root.iterdir()), before)

    def test_filesystem_root_report_is_published_after_final_snapshot(self):
        report = self.root / "new-directory/report.json"
        snapshots = []

        def snapshot(root):
            self.assertEqual(root, Path("/"))
            self.assertFalse(report.parent.exists())
            snapshots.append(root)
            return {}

        argv = [
            "bench",
            "--root",
            "/",
            "--tools",
            "--case",
            "paths",
            "--runs",
            "1",
            "--warmups",
            "0",
            "--output",
            str(report),
        ]
        # Exercise orchestration without scanning the host filesystem.
        with (
            patch.object(sys, "argv", argv),
            patch.object(bench, "inventory", side_effect=snapshot),
            patch.object(bench, "verify", return_value=[]),
            patch.object(bench, "run", return_value=(1.0, b"")),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(bench.main(), 0)
        self.assertEqual(len(snapshots), 2)
        self.assertEqual(json.loads(report.read_text())["root"], "/")

    def test_original_query_relative_paths_include_filesystem_root(self):
        template = (bench.REPO / "benchmarks/queries/hell.fsql").read_text()
        inventory_sql = template.split("inventory AS (", 1)[1].split(
            "),\ncandidates", 1
        )[0]
        for root in ["/", "/tmp/project", "/tmp/quote'root"]:
            path = root.rstrip("/") + "/build/file"
            # Run the actual template projection over a constant row instead of files('/').
            sql = inventory_sql.replace("{{root}}", bench.sql_string(root)).replace(
                f"files({bench.sql_string(root)})",
                f"(SELECT {bench.sql_string(path)} AS path, 'file' AS name, "
                "'build' AS parent, 'file' AS kind) AS fixture",
            )
            result = subprocess.run(
                [str(bench.REPO / "target/release/fsql"), "-f", "json", "-e=" + sql],
                capture_output=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(json.loads(result.stdout)["relative_path"], "build/file")

    def test_invalid_utf8_is_rejected(self):
        import os

        descriptor = os.open(
            os.fsencode(self.root) + b"/invalid-\xff", os.O_CREAT | os.O_WRONLY, 0o600
        )
        os.close(descriptor)
        with self.assertRaisesRegex(ValueError, "UTF-8"):
            bench.inventory(self.root)


if __name__ == "__main__":
    unittest.main()
