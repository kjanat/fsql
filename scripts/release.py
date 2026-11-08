"""Publish fsql-sqlparser, fsql, and fsql-cli for one approved Beepboop commit."""

import hashlib
import json
import os
import tarfile
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

REPOSITORY = "kjanat/fsql"
STREAMS = {
    "main": {
        "crates": ("fsql-sqlparser", "fsql"),
        "tag": "v",
        "manifests": {
            "fsql-sqlparser": "vendor/sqlparser/Cargo.toml",
            "fsql": "crates/fsql/Cargo.toml",
        },
    },
    "cli": {
        "crates": ("fsql-cli",),
        "tag": "cli-v",
        "manifests": {"fsql-cli": "crates/fsql-cli/Cargo.toml"},
    },
}
WORKFLOWS = ("ci.yml", "grammar.yml")
ORIGIN = "https://github.com/kjanat/fsql"
USER_AGENT = f"fsql-release ({ORIGIN})"


class ReleaseRejected(Exception):
    pass


class ReleaseNotVisible(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ReleaseRejected(message)


def parse_context(payload):
    context = json.loads(payload)
    require(isinstance(context, dict), "Release context is not an object")
    sha = context.get("sourceSha")
    version = context.get("version")
    draft = context.get("draftRelease")
    require(context.get("repository") == REPOSITORY, "Repository mismatch")
    require(context.get("operation") == "promote", "Operation mismatch")
    stream = context.get("stream")
    require(stream in STREAMS, "Stream mismatch")
    require(
        isinstance(sha, str)
        and len(sha) == 40
        and all(c in "0123456789abcdef" for c in sha),
        "Expected a commit SHA",
    )
    require(
        isinstance(version, str) and version.count(".") == 2 and "+" not in version,
        "Expected a release version",
    )
    require(
        isinstance(draft, dict)
        and draft.get("tag") == f"{STREAMS[stream]['tag']}{version}",
        "Draft release tag differs from the approved version",
    )
    require(isinstance(draft.get("id"), int), "Draft release id is missing")
    return context


def workspace_version(source):
    version = tomllib.loads(source)["workspace"]["package"]["version"]
    require(isinstance(version, str), "Workspace version is missing")
    return version


def inherited(source):
    version = tomllib.loads(source)["package"]["version"]
    return version == {"workspace": True}


def manifest_version(source):
    version = tomllib.loads(source)["package"]["version"]
    require(isinstance(version, str), "Package version is missing")
    return version


def request_json(url, token=None):
    headers = {"User-Agent": USER_AGENT, "Accept": "application/json"}
    if token is not None:
        headers["Authorization"] = "Bearer " + token
        headers["Accept"] = "application/vnd.github+json"
        headers["X-GitHub-Api-Version"] = "2026-03-10"
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def commit_verified(commit, sha):
    verification = commit["commit"]["verification"]
    return commit["sha"] == sha and verification["verified"] is True


def workflow_succeeded(body, sha):
    runs = [
        run
        for run in body["workflow_runs"]
        if run["head_sha"] == sha
        and run["head_branch"] == "master"
        and run["event"] == "push"
    ]
    if not runs:
        return False
    latest = max(runs, key=lambda run: (run["id"], run["run_attempt"]))
    return latest["status"] == "completed" and latest["conclusion"] == "success"


def read_crate(path, name, version, sha):
    prefix = f"{name}-{version}/"
    with tarfile.open(path, "r:gz") as archive:
        vcs_file = archive.extractfile(prefix + ".cargo_vcs_info.json")
        manifest_file = archive.extractfile(prefix + "Cargo.toml")
        require(
            vcs_file is not None and manifest_file is not None,
            f"{name} archive is missing its manifest",
        )
        vcs = json.loads(vcs_file.read())
        manifest = tomllib.loads(manifest_file.read().decode())
    require(vcs["git"]["sha1"] == sha, f"{name} archive commit mismatch")
    require(
        vcs["git"].get("dirty") is not True,
        f"{name} archive contains uncommitted changes",
    )
    require(manifest["package"]["name"] == name, f"{name} archive package mismatch")
    require(
        manifest["package"]["version"] == version, f"{name} archive version mismatch"
    )
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def registry_version(name, version, fetch=request_json):
    return fetch(f"https://crates.io/api/v1/crates/{name}/{version}")


def stream_crates(context):
    return STREAMS[context["stream"]]["crates"]


def pending_crates(version, names, fetch=request_json):
    pending = []
    for name in names:
        body = registry_version(name, version, fetch)
        if body is None:
            pending.append(name)
            continue
        entry = body["version"]
        require(
            entry["crate"] == name
            and entry["num"] == version
            and entry["yanked"] is False,
            f"Existing registry publication differs from the approved version: {name}",
        )
    return pending


def publications(version, sha, directory, names, fetch=request_json):
    rows = []
    for name in names:
        digest = read_crate(directory / f"{name}-{version}.crate", name, version, sha)
        body = registry_version(name, version, fetch)
        if body is None:
            raise ReleaseNotVisible(name)
        entry = body["version"]
        require(
            entry["crate"] == name
            and entry["num"] == version
            and entry["yanked"] is False
            and entry["checksum"] == digest,
            f"Existing registry publication differs from the verified archive: {name}",
        )
        rows.append({
            "id": f"cargo:{name}",
            "manager": "cargo",
            "name": name,
            "version": version,
            "outcome": "succeeded",
            "required": True,
            "digest": digest,
        })
    return rows


def git_show(sha, path):
    import subprocess

    result = subprocess.run(
        ["git", "show", f"{sha}:{path}"], check=True, capture_output=True, text=True
    )
    return result.stdout


def approve(context, repository_id, show=git_show, fetch=request_json, token=None):
    require(
        str(context["repositoryId"]) == str(repository_id), "Repository ID mismatch"
    )
    sha = context["sourceSha"]
    version = context["version"]
    selected = STREAMS[context["stream"]]
    if context["stream"] == "main":
        require(
            workspace_version(show(sha, "Cargo.toml")) == version,
            "Manifest version differs from approval",
        )
        for path in selected["manifests"].values():
            require(
                inherited(show(sha, path)),
                f"{path} does not inherit the workspace version",
            )
    else:
        path = selected["manifests"]["fsql-cli"]
        source = show(sha, path)
        require(not inherited(source), "fsql-cli version must be a literal")
        require(
            manifest_version(source) == version,
            "CLI version differs from approval",
        )
    commit = fetch(f"https://api.github.com/repos/{REPOSITORY}/commits/{sha}", token)
    require(
        commit is not None and commit_verified(commit, sha),
        "Release commit has no verified signature",
    )
    for workflow in WORKFLOWS:
        body = fetch(
            f"https://api.github.com/repos/{REPOSITORY}/actions/workflows/{workflow}/runs?head_sha={sha}&branch=master&event=push&per_page=100",
            token,
        )
        require(
            body is not None and workflow_succeeded(body, sha),
            f"{workflow} must succeed on the approved commit",
        )


def main(argv):
    command = argv[1] if len(argv) > 1 else ""
    context = parse_context(os.environ["RELEASE_CONTEXT"])
    if command == "check":
        approve(context, os.environ["GITHUB_REPOSITORY_ID"])
        require(os.environ["GITHUB_REPOSITORY"] == REPOSITORY, "Repository mismatch")
        return
    directory = Path("target/package")
    names = stream_crates(context)
    if command == "crates":
        for name in names:
            print(name)
        return
    if command == "pending":
        for name in pending_crates(context["version"], names):
            print(name)
        return
    if command == "report":
        rows = publications(context["version"], context["sourceSha"], directory, names)
        line = json.dumps(rows, separators=(",", ":"))
        output = os.environ.get("GITHUB_OUTPUT")
        if output:
            with open(output, "a", encoding="utf-8") as handle:
                handle.write(f"publications={line}\n")
        print(line)
        return
    raise SystemExit("usage: release.py check|crates|pending|report")


if __name__ == "__main__":
    try:
        main(__import__("sys").argv)
    except ReleaseNotVisible as error:
        raise SystemExit(2) from error
    except ReleaseRejected as error:
        raise SystemExit(f"::error::{error}") from error
