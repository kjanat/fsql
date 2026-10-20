#!/usr/bin/env python3
"""Correctness-gated, warm-cache filesystem CLI benchmarks (Python stdlib only)."""

import argparse
import hashlib
import json
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib
from collections import Counter
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CASES = ("paths", "shallow", "size", "suffix", "exclude")
EXCLUDED = {".git", "target", "node_modules"}


@dataclass
class Command:
    name: str
    case: str
    argv: list[str]
    codec: str
    stdin: bytes | None = None


def sql_string(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def executable(value: str) -> str:
    resolved = shutil.which(value)
    if resolved is None:
        raise ValueError(f"executable not found: {value}")
    return str(Path(resolved).absolute())


def publish_report(output: Path, report: dict) -> None:
    """Replace a report only after a complete write; preserve it on failure."""
    payload = json.dumps(report, indent=2) + "\n"
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=output.parent,
            prefix=f".{output.name}.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            temporary = Path(stream.name)
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def assignment(value: str) -> tuple[str, str]:
    name, sep, path = value.partition("=")
    if not sep or not name or not path:
        raise argparse.ArgumentTypeError("expected LABEL=PATH")
    return name, path


def positive(value: str) -> int:
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def inventory(root: Path) -> dict[str, tuple[bool, int, tuple[int, ...]]]:
    """Independent oracle: relative path -> (regular file, lstat size)."""
    result = {}
    pending = [root]
    while pending:
        with os.scandir(pending.pop()) as entries:
            for entry in entries:
                path = Path(entry.path)
                relative = path.relative_to(root).as_posix()
                try:
                    relative.encode("utf-8")
                except UnicodeEncodeError as error:
                    raise ValueError(
                        "benchmark requires UTF-8 filenames; refusing lossy comparison"
                    ) from error
                # fsql intentionally hides its private recovery staging entries.
                if any(
                    re.fullmatch(r"\.fsql-[0-9a-fA-F]+-[0-9a-fA-F]+-[0-9]+", part)
                    for part in path.relative_to(root).parts
                ):
                    raise ValueError(
                        "private .fsql-<hex>-<hex>-<decimal> staging names require a separate benchmark root"
                    )
                stat = entry.stat(follow_symlinks=False)
                result[relative] = (
                    entry.is_file(follow_symlinks=False),
                    stat.st_size,
                    (
                        stat.st_dev,
                        stat.st_ino,
                        stat.st_mode,
                        stat.st_mtime_ns,
                        stat.st_ctime_ns,
                    ),
                )
                if entry.is_dir(follow_symlinks=False):
                    pending.append(path)
    return result


def expected(
    rows: dict[str, tuple[bool, int, tuple[int, ...]]], case: str, size: int
) -> Counter[str]:
    return Counter(
        path
        for path, (regular, length, _) in rows.items()
        if (
            case == "paths"
            or case == "shallow"
            and "/" not in path
            or case == "size"
            and regular
            and length > size
            or case == "suffix"
            and path.endswith(".rs")
            or case == "exclude"
            and not (set(path.split("/")) & EXCLUDED)
        )
    )


def fsql_command(
    name: str, binary: str, case: str, root: Path, size: int, legacy: bool
) -> Command:
    relative = f"substr(path, {len(str(root)) + (1 if str(root) == '/' else 2)})"
    predicate = {
        "paths": "depth > 0",
        "shallow": "depth = 1",
        "size": f"depth > 0 AND kind = 'file' AND size > {size}",
        "suffix": "depth > 0 AND path REGEXP '[.]rs$'",
        "exclude": "depth > 0 AND NOT (relative_path GLOB ANY (ARRAY["
        "'**/.git', '**/.git/**', '**/target', '**/target/**',"
        "'**/node_modules', '**/node_modules/**']))",
    }[case]
    if case == "exclude" and legacy:
        predicate = f"depth > 0 AND NOT ({relative} REGEXP '(^|/)([.]git|target|node_modules)(/|$)')"
    sql = "SELECT path FROM files WHERE " + predicate
    argv = [binary, "-C", str(root), "-f", "json", "-e=" + sql]
    if case == "shallow":
        argv += ["--max-depth", "1"]
    return Command(name, case, argv, "json")


def tool_command(
    name: str, binary: str, case: str, root: Path, size: int, threads: int | None
) -> Command | None:
    if name in ("find", "bfs"):
        argv = [binary, "-P", ".", "-mindepth", "1"]
        if case == "shallow":
            argv += ["-maxdepth", "1"]
        elif case == "size":
            argv += ["-type", "f", "-size", f"+{size}c"]
        elif case == "suffix":
            argv += ["-name", "*.rs"]
        elif case == "exclude":
            argv += [
                "(",
                "-name",
                ".git",
                "-o",
                "-name",
                "target",
                "-o",
                "-name",
                "node_modules",
                ")",
                "-prune",
                "-o",
            ]
        return Command(name, case, argv + ["-print0"], "nul")
    if name == "fselect":
        query = "path from . nogitignore nohgignore nodockerignore"
        if case == "shallow":
            query += " maxdepth 1"
        elif case == "size":
            query += f" where is_file = true and is_symlink = false and size > {size}"
        elif case == "suffix":
            query += " where path =~ '[.]rs$'"
        elif case == "exclude":
            query += " where path notrx '(^|/)([.]git|target|node_modules)(/|$)'"
        return Command(
            name,
            case,
            [
                binary,
                "--config",
                str(REPO / "benchmarks/fselect.toml"),
                query + " into list",
            ],
            "nul",
        )
    if name == "fd":
        argv = [
            binary,
            "--hidden",
            "--no-ignore",
            "--case-sensitive",
            "--color",
            "never",
            "--print0",
        ]
        if threads is not None:
            argv += ["--threads", str(threads)]
        if case == "shallow":
            argv += ["--max-depth", "1"]
        elif case == "size":
            argv += ["--type", "f", "--size", f"+{size + 1}b"]
        elif case == "exclude":
            for component in sorted(EXCLUDED):
                argv += ["--exclude", component]
        return Command(
            name, case, argv + ["[.]rs$" if case == "suffix" else "", "."], "nul"
        )
    if name == "ls":
        if case != "shallow":
            return None
        return Command(
            name,
            case,
            [
                binary,
                "-A",
                "-1",
                "--zero",
                "--sort=none",
                "--color=never",
                "--quoting-style=literal",
                "--",
                ".",
            ],
            "nul",
        )
    if name == "gahag":
        # gahag has no kind column, so cannot express our regular-file size case.
        if case == "size":
            return None
        predicate = {
            "paths": "",
            "shallow": "",
            "suffix": ' where name =~ "[.]rs$"',
            "exclude": ' where !(name =~ "(^|/)([.]git|target|node_modules)(/|$)")',
        }[case]
        query = (
            "select name"
            + ("" if case == "shallow" else " recursive")
            + " from ."
            + predicate
        )
        return Command(name, case, [binary], "gahag", (query + "\nquit\n").encode())
    raise ValueError(f"unknown adapter: {name}")


def adapter_commands(
    files: list[Path], cases: list[str], root: Path, size: int
) -> list[Command]:
    """Trusted TOML argv templates; no shell expansion or command-string parsing."""
    commands = []
    for file in files:
        config = tomllib.loads(file.read_text())
        name = config["name"]
        if not isinstance(name, str) or not name:
            raise ValueError(f"{file}: adapter name must be a nonempty string")
        for case, spec in config["cases"].items():
            if case not in CASES:
                raise ValueError(f"{file}: unknown case {case}")
            if case not in cases:
                continue
            argv = spec["argv"]
            codec = spec.get("codec", "nul")
            if (
                codec not in ("nul", "json")
                or not isinstance(argv, list)
                or not argv
                or not all(isinstance(arg, str) for arg in argv)
            ):
                raise ValueError(
                    f"{file}: expected argv string array and nul/json codec"
                )
            argv = [
                arg
                .replace("{{root}}", str(root))
                .replace("{{size}}", str(size))
                .replace("{{size_plus_one}}", str(size + 1))
                for arg in argv
            ]
            argv[0] = executable(argv[0])
            commands.append(Command(name, case, argv, codec))
    return commands


def decode(command: Command, output: bytes, root: Path) -> list[str]:
    if command.codec == "json":
        paths = []
        for line in output.splitlines():
            row = json.loads(line)
            if set(row) != {"path"} or not isinstance(row["path"], str):
                raise ValueError("SQL must return exactly one text column named path")
            path = Path(row["path"])
            if not path.is_absolute():
                raise ValueError(f"expected absolute SQL path: {path}")
            paths.append(path.relative_to(root).as_posix())
        return paths
    if command.codec == "nul":
        if output and not output.endswith(b"\0"):
            raise ValueError("missing NUL record terminator")
        paths = output.decode("utf-8").split("\0")[:-1]
        return [
            Path(path).relative_to(root).as_posix()
            if Path(path).is_absolute()
            else path.removeprefix("./").removesuffix("/")
            for path in paths
        ]
    if not command.stdin:
        raise ValueError("missing gahag REPL query input")
    lines = output.decode("utf-8").split("\n")
    query = command.stdin.decode().splitlines()[0]
    if not lines or lines[0] not in (query, "fsql> " + query):
        raise ValueError("unexpected gahag REPL framing")
    lines = lines[1:]
    if lines and lines[-1] == "":
        lines.pop()
    if lines and lines[-1] == "fsql> ":
        lines.pop()
    if lines and lines[-1] == "":
        lines.pop()
    return lines


def run(
    command: Command, root: Path, timeout: float, capture: bool
) -> tuple[float, bytes]:
    start = time.perf_counter_ns()
    result = subprocess.run(
        command.argv,
        check=False,
        cwd=root,
        input=command.stdin,
        stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        timeout=timeout,
        env={**os.environ, "LC_ALL": "C.UTF-8", "NO_COLOR": "1"},
    )
    elapsed = (time.perf_counter_ns() - start) / 1_000_000
    if result.returncode or result.stderr:
        raise ValueError(
            f"{command.name}/{command.case}: exit {result.returncode}: {result.stderr.decode(errors='replace')}"
        )
    return elapsed, result.stdout or b""


def verify(
    command: Command, root: Path, timeout: float, reference: Counter[str]
) -> list[str]:
    _, output = run(command, root, timeout, True)
    paths = decode(command, output, root)
    actual = Counter(paths)
    if actual != reference:
        missing = list((reference - actual).items())[:5]
        extra = list((actual - reference).items())[:5]
        raise ValueError(
            f"{command.name}/{command.case}: result mismatch; missing={missing!r}, extra={extra!r}"
        )
    return paths


def fixture(root: Path, files: int) -> None:
    for index in range(files):
        parent = root / ("target" if index % 3 == 0 else "src") / f"group-{index % 10}"
        parent.mkdir(parents=True, exist_ok=True)
        path = parent / f"file-{index:06d}.{'rs' if index % 2 else 'txt'}"
        with path.open("wb") as stream:
            stream.truncate(1_000_001 if index % 7 == 0 else 64)
    for name in [
        ".hidden",
        "space name.rs",
        "quote'name.rs",
        "unicode-λ.rs",
        "target.rs",
    ]:
        (root / name).write_text("fixture\n")
    for directory in ["node_modules", ".git", "empty", "nested/target"]:
        (root / directory).mkdir(parents=True, exist_ok=True)
    (root / "nested/target/ignored.rs").write_text("ignored\n")
    (root / "symlink").symlink_to("src", target_is_directory=True)
    (root / "broken").symlink_to("missing")


def digest(path: str) -> str:
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main() -> int:
    harness_path = str(Path(__file__).resolve())
    harness_hash = digest(harness_path)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        help="existing read-only input tree; default: generated fixture",
    )
    parser.add_argument(
        "--files", type=positive, default=2000, help="generated fixture file count"
    )
    parser.add_argument(
        "--fsql", action="append", type=assignment, default=[], metavar="LABEL=PATH"
    )
    parser.add_argument(
        "--fsql-legacy",
        action="append",
        type=assignment,
        default=[],
        metavar="LABEL=PATH",
        help="pre-PR8 executable; exclusion uses a regex",
    )
    parser.add_argument(
        "--tools",
        nargs="*",
        choices=["find", "fd", "ls", "bfs", "fselect"],
        default=["find", "fd", "ls", "bfs", "fselect"],
    )
    parser.add_argument(
        "--adapter",
        action="append",
        type=Path,
        default=[],
        help="additional trusted TOML command adapter",
    )
    parser.add_argument("--gahag", help="optional prebuilt gahag/FSQL executable")
    parser.add_argument("--case", action="append", choices=CASES, dest="cases")
    parser.add_argument(
        "--query",
        action="append",
        type=assignment,
        default=[],
        metavar="LABEL=FILE",
        help="compare equivalent SELECT templates on each modern fsql; replaces built-in cases",
    )
    parser.add_argument(
        "--ordered",
        action="store_true",
        help="also require identical order for custom queries",
    )
    parser.add_argument("--runs", type=positive, default=7)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument(
        "--size",
        type=int,
        default=1_000_000,
        help="strict lower bound in bytes for regular files",
    )
    parser.add_argument(
        "--fd-threads",
        type=positive,
        help="default: fd chooses its normal thread count",
    )
    parser.add_argument(
        "--seed", type=int, default=0, help="deterministic shuffled execution order"
    )
    parser.add_argument(
        "--output", type=Path, default=REPO / "target/benchmarks/latest.json"
    )
    args = parser.parse_args()
    if args.warmups < 0 or args.timeout <= 0 or args.size < 0:
        parser.error("warmups and size must be nonnegative; timeout must be positive")
    args.output = args.output.resolve()
    binaries = [(name, executable(path), False) for name, path in args.fsql]
    binaries += [(name, executable(path), True) for name, path in args.fsql_legacy]
    if not binaries:
        binaries = [("fsql", executable(str(REPO / "target/release/fsql")), False)]
    labels = (
        [name for name, _, _ in binaries]
        + args.tools
        + (["gahag"] if args.gahag else [])
    )
    if len(labels) != len(set(labels)):
        parser.error("executable labels must be unique and not collide with tool names")
    with tempfile.TemporaryDirectory(prefix="fsql-bench-") as temporary:
        root = args.root.resolve(strict=True) if args.root else Path(temporary) / "tree"
        if not args.root:
            root.mkdir()
            fixture(root, args.files)
        if root != Path(root.anchor) and args.output.is_relative_to(root):
            parser.error("output must be outside the measured tree")
        before = inventory(root)
        commands = []
        skipped = []
        references = {}
        if args.query:
            if args.fsql_legacy or args.cases or args.adapter:
                parser.error(
                    "--query cannot be combined with --fsql-legacy, --case or --adapter"
                )
            names = [name for name, _ in args.query]
            if len(names) != len(set(names)):
                parser.error("query labels must be unique")
            for label, file in args.query:
                sql = Path(file).read_text().replace("{{root}}", sql_string(str(root)))
                for name, binary, _ in binaries:
                    commands.append(
                        Command(
                            name,
                            label,
                            [binary, "-C", str(root), "-f", "json", "-e=" + sql],
                            "json",
                        )
                    )
            _, output = run(commands[0], root, args.timeout, True)
            ordered_reference = decode(commands[0], output, root)
            references = {label: Counter(ordered_reference) for label, _ in args.query}
        else:
            for case in dict.fromkeys(args.cases or CASES):
                references[case] = expected(before, case, args.size)
                commands += [
                    fsql_command(name, binary, case, root, args.size, legacy)
                    for name, binary, legacy in binaries
                ]
                tools = [
                    (name, executable(found) if (found := shutil.which(name)) else None)
                    for name in args.tools
                ]
                if args.gahag:
                    tools.append(("gahag", executable(args.gahag)))
                for name, binary in tools:
                    command = (
                        tool_command(
                            name, binary, case, root, args.size, args.fd_threads
                        )
                        if binary
                        else None
                    )
                    if command is None:
                        skipped.append({
                            "tool": name,
                            "case": case,
                            "reason": "not installed"
                            if binary is None
                            else "unsupported semantics",
                        })
                    elif name == "gahag" and any(
                        "\n" in p or "\r" in p for p in before
                    ):
                        skipped.append({
                            "tool": name,
                            "case": case,
                            "reason": "line-based output cannot represent these filenames",
                        })
                    else:
                        commands.append(command)
        commands.extend(
            adapter_commands(
                args.adapter, list(dict.fromkeys(args.cases or CASES)), root, args.size
            )
        )
        identities = [(c.name, c.case) for c in commands]
        if len(identities) != len(set(identities)):
            raise ValueError(
                "duplicate tool/case identity; choose unique adapter labels"
            )
        binary_paths = sorted({c.argv[0] for c in commands})
        binary_hashes = {path: digest(path) for path in binary_paths}
        for command in commands:
            paths = verify(command, root, args.timeout, references[command.case])
            if args.query and args.ordered and paths != ordered_reference:
                raise ValueError(f"{command.name}/{command.case}: order mismatch")
            print(
                f"verified {command.name}/{command.case}: {len(paths)} rows", flush=True
            )
        samples = {(c.name, c.case): [] for c in commands}
        rng = random.Random(args.seed)
        for iteration in range(args.warmups + args.runs):
            order = commands.copy()
            rng.shuffle(order)
            for command in order:
                elapsed, _ = run(command, root, args.timeout, False)
                if iteration >= args.warmups:
                    samples[command.name, command.case].append(elapsed)
        # Recheck both results and tree metadata: reject changing inputs, not just errors.
        for command in commands:
            paths = verify(command, root, args.timeout, references[command.case])
            if args.query and args.ordered and paths != ordered_reference:
                raise ValueError(f"{command.name}/{command.case}: order changed")
        if inventory(root) != before:
            raise ValueError("input tree changed during benchmark; results discarded")
        results = []
        for command in commands:
            values = samples[command.name, command.case]
            results.append({
                "tool": command.name,
                "case": command.case,
                "rows": sum(references[command.case].values()),
                "median_ms": statistics.median(values),
                "min_ms": min(values),
                "max_ms": max(values),
                "samples_ms": values,
                "argv": command.argv,
                "stdin": command.stdin.decode() if command.stdin else None,
                "output_codec": command.codec,
            })
        if any(digest(path) != binary_hashes[path] for path in binary_paths):
            raise ValueError(
                "an executable changed during the benchmark; results discarded"
            )
        if digest(harness_path) != harness_hash:
            raise ValueError("harness changed during the benchmark; results discarded")
        report = {
            "schema_version": 1,
            "timestamp": datetime.now(UTC).isoformat(),
            "platform": platform.platform(),
            "cpu_count": os.cpu_count(),
            "harness_sha256": harness_hash,
            "timeout_seconds": args.timeout,
            "root": str(root),
            "fixture_files": args.files if not args.root else None,
            "generated_fixture": not bool(args.root),
            "runs": args.runs,
            "warmups": args.warmups,
            "seed": args.seed,
            "fd_threads": args.fd_threads,
            "cache": "warm; no cache flush",
            "verification": "before and after; independent filesystem oracle"
            if not args.query
            else "before and after; first custom query is reference",
            "binaries": {
                path: {"sha256": binary_hashes[path]} for path in binary_paths
            },
            "skipped": skipped,
            "results": results,
        }
        # For a filesystem-root scan there is no outside destination. Publish
        # only after all result, input-tree, and executable checks have passed.
        publish_report(args.output, report)
        print(
            "\ncase                 tool                 rows     median ms       min       max"
        )
        for result in results:
            print(
                f"{result['case']:20} {result['tool']:20} {result['rows']:8} {result['median_ms']:13.3f} {result['min_ms']:9.3f} {result['max_ms']:9.3f}"
            )
        for skip in skipped:
            print(f"skipped {skip['tool']}/{skip['case']}: {skip['reason']}")
        print(f"\nReport: {args.output}")
    return 0


def cli() -> int:
    try:
        return main()
    except (ValueError, OSError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(cli())
