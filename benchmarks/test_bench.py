"""Contract tests use installed tools and fail closed on divergent output."""

import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

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
            for tool in ["find", "fd", "ls"]:
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
        self.assertEqual(len(data["results"]), 6)
        self.assertTrue(all(len(row["samples_ms"]) == 1 for row in data["results"]))
        self.assertTrue(
            all(len(binary["sha256"]) == 64 for binary in data["binaries"].values())
        )

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
