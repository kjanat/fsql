#!/usr/bin/env python3
"""Exercise the runner's filesystem boundary with real Bubblewrap processes."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "scripts/sandbox-runner.sh"


class SandboxTests(unittest.TestCase):
    def invoke(self, script, *args, seed=None):
        env = os.environ.copy()
        env.pop("FSQL_SANDBOX_SEED", None)
        if seed is not None:
            env["FSQL_SANDBOX_SEED"] = str(seed)
        return subprocess.run(
            [str(RUNNER), "/bin/sh", "-eu", "-c", script, "probe", *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def test_writable_tree_is_disposable_and_arguments_survive(self):
        first = self.invoke(
            'printf "%s" "$1" > note; cat note', "spaces ' and $literal"
        )
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stdout, "spaces ' and $literal")
        second = self.invoke("""
            test ! -e note
            test "$PWD" = /tmp/fsql-sandbox
            test "$HOME" = /tmp/fsql-home
            test "$XDG_DATA_HOME" = /tmp/fsql-home/.local/share
            test "$TMPDIR" = /tmp
        """)
        self.assertEqual(second.returncode, 0, second.stderr)

    def test_host_and_symlink_targets_are_read_only(self):
        # Outside /tmp so the file remains visible through the read-only root.
        with tempfile.TemporaryDirectory(prefix=".sandbox-probe-", dir=ROOT) as tmp:
            host = Path(tmp) / "host.txt"
            host.write_text("original")
            result = self.invoke(
                """
                cat "$1"
                if printf changed > "$1"; then exit 10; fi
                if rm "$1"; then exit 11; fi
                ln -s "$1" escape
                if printf changed > escape; then exit 12; fi
            """,
                str(host),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "original")
            self.assertEqual(host.read_text(), "original")

    def test_seed_is_copied_including_symlinks(self):
        with tempfile.TemporaryDirectory(prefix="fsql seed ") as tmp:
            seed = Path(tmp)
            (seed / "note").write_text("original")
            (seed / "link").symlink_to("note")
            result = self.invoke(
                """
                test -L link
                printf changed > link
                test "$(cat note)" = changed
                test "$(cat /tmp/fsql-seed/note)" = original
                rm note
            """,
                seed=seed,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((seed / "note").read_text(), "original")

    def test_exit_status_is_preserved(self):
        result = self.invoke("exit 37")
        self.assertEqual(result.returncode, 37, result.stderr)

    def test_invalid_seed_fails_before_execution(self):
        result = self.invoke(
            "echo should-not-run", seed="/definitely/missing/fsql-seed"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
