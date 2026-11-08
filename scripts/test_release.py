import hashlib
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

import release


def context(**extra):
    body = {
        "repository": release.REPOSITORY,
        "repositoryId": 7,
        "operation": "promote",
        "stream": "main",
        "sourceSha": "a" * 40,
        "version": "0.1.0",
        "draftRelease": {"id": 4, "tag": "v0.1.0"},
    }
    body.update(extra)
    return body


def crate(directory, name, version, sha, dirty=False):
    prefix = f"{name}-{version}/"
    manifest = f'[package]\nname = "{name}"\nversion = "{version}"\n'
    vcs = json.dumps({"git": {"sha1": sha, **({"dirty": True} if dirty else {})}})
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for path, text in (
            (prefix + "Cargo.toml", manifest),
            (prefix + ".cargo_vcs_info.json", vcs),
        ):
            payload = text.encode()
            info = tarfile.TarInfo(path)
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
    path = directory / f"{name}-{version}.crate"
    path.write_bytes(buffer.getvalue())
    return hashlib.sha256(path.read_bytes()).hexdigest()


class ReleasePlan(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.directory = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_context_accepts_the_approved_release(self):
        parsed = release.parse_context(json.dumps(context()))
        self.assertEqual(parsed["version"], "0.1.0")

    def test_context_rejects_a_different_tag(self):
        body = context()
        body["draftRelease"]["tag"] = "v0.2.0"
        with self.assertRaises(release.ReleaseRejected):
            release.parse_context(json.dumps(body))

    def test_workspace_version_and_inheritance(self):
        self.assertEqual(
            release.workspace_version('[workspace.package]\nversion = "0.1.0"\n'),
            "0.1.0",
        )
        self.assertTrue(
            release.inherited("[package]\nversion = { workspace = true }\n")
        )
        self.assertTrue(release.inherited("[package]\nversion.workspace = true\n"))
        self.assertFalse(release.inherited('[package]\nversion = "0.1.0"\n'))

    def test_workflow_success_uses_the_latest_run(self):
        sha = "b" * 40
        body = {
            "workflow_runs": [
                {
                    "id": 1,
                    "run_attempt": 1,
                    "head_sha": sha,
                    "head_branch": "master",
                    "event": "push",
                    "status": "completed",
                    "conclusion": "failure",
                },
                {
                    "id": 2,
                    "run_attempt": 1,
                    "head_sha": sha,
                    "head_branch": "master",
                    "event": "push",
                    "status": "completed",
                    "conclusion": "success",
                },
            ]
        }
        self.assertTrue(release.workflow_succeeded(body, sha))
        self.assertFalse(release.workflow_succeeded({"workflow_runs": []}, sha))

    def test_signed_commit_must_match_the_approved_sha(self):
        sha = "a" * 40
        commit = {"sha": sha, "commit": {"verification": {"verified": True}}}
        self.assertTrue(release.commit_verified(commit, sha))
        commit["commit"]["verification"]["verified"] = False
        self.assertFalse(release.commit_verified(commit, sha))

    def test_pending_skips_a_matching_publication_and_keeps_order(self):
        seen = {}

        def fetch(url, token=None):
            name = url.rstrip("/").split("/")[-2]
            seen[name] = url
            if name == "fsql":
                return None
            return {
                "version": {
                    "crate": name,
                    "num": "0.1.0",
                    "yanked": False,
                    "checksum": "ab",
                }
            }

        names = release.STREAMS["main"]["crates"]
        self.assertEqual(release.pending_crates("0.1.0", names, fetch), ["fsql"])
        self.assertEqual(list(seen), list(names))

    def test_pending_rejects_a_yanked_publication(self):
        def fetch(url, token=None):
            return {
                "version": {
                    "crate": "fsql-sqlparser",
                    "num": "0.1.0",
                    "yanked": True,
                    "checksum": "ab",
                }
            }

        with self.assertRaises(release.ReleaseRejected):
            release.pending_crates("0.1.0", ("fsql-sqlparser",), fetch)

    def test_archive_rejects_a_dirty_tree(self):
        directory = self.directory
        crate(directory, "fsql-sqlparser", "0.1.0", "e" * 40, dirty=True)
        with self.assertRaises(release.ReleaseRejected):
            release.read_crate(
                directory / "fsql-sqlparser-0.1.0.crate", "fsql-sqlparser", "0.1.0", "e" * 40
            )

    def test_report_requires_every_crate(self):
        sha = "f" * 40
        directory = self.directory
        names = release.STREAMS["main"]["crates"]
        digests = {name: crate(directory, name, "0.1.0", sha) for name in names}

        def fetch(url, token=None):
            name = url.rstrip("/").split("/")[-2]
            return {
                "version": {
                    "crate": name,
                    "num": "0.1.0",
                    "yanked": False,
                    "checksum": digests[name],
                }
            }

        rows = release.publications("0.1.0", sha, directory, names, fetch)
        self.assertEqual(
            [row["id"] for row in rows], [f"cargo:{name}" for name in names]
        )

    def test_report_rejects_a_different_checksum(self):
        sha = "d" * 40
        directory = self.directory
        crate(directory, "fsql-sqlparser", "0.1.0", sha)

        def fetch(url, token=None):
            name = url.rstrip("/").split("/")[-2]
            if name != "fsql-sqlparser":
                return None
            return {
                "version": {
                    "crate": name,
                    "num": "0.1.0",
                    "yanked": False,
                    "checksum": "0" * 64,
                }
            }

        with self.assertRaises(release.ReleaseRejected):
            release.publications(
                "0.1.0", sha, directory, release.STREAMS["main"]["crates"], fetch
            )

    def test_report_waits_when_a_crate_is_not_visible(self):
        sha = "9" * 40
        directory = self.directory
        names = release.STREAMS["main"]["crates"]
        for name in names:
            crate(directory, name, "0.1.0", sha)

        def fetch(url, token=None):
            return None

        with self.assertRaises(release.ReleaseNotVisible):
            release.publications("0.1.0", sha, directory, names, fetch)

    def test_cli_tag_uses_its_own_prefix(self):
        body = context(stream="cli")
        body["draftRelease"]["tag"] = "v0.1.0"
        with self.assertRaises(release.ReleaseRejected):
            release.parse_context(json.dumps(body))
        body["draftRelease"]["tag"] = "cli-v0.1.0"
        self.assertEqual(release.parse_context(json.dumps(body))["stream"], "cli")

    def test_cli_rejects_a_workspace_version(self):
        body = context(stream="cli")
        body["draftRelease"]["tag"] = "cli-v0.1.0"
        parsed = release.parse_context(json.dumps(body))

        def show(sha, path):
            return "[package]\nversion = { workspace = true }\n"

        with self.assertRaises(release.ReleaseRejected):
            release.approve(parsed, 7, show=show, fetch=lambda url, token=None: None)


if __name__ == "__main__":
    unittest.main()
