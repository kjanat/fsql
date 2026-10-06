# Development

The commands below use Cargo and Python directly. Contributors using [Runner] can also invoke the repository's Cargo aliases and package scripts with `run fsql`, `run example:query crates`, `run bench:engine`, or `run sandbox:check`. Runner is optional. Cargo aliases are defined in [`.cargo/config.toml`].

## Examples and benchmarks

See [examples] for runnable SQL and Rust library examples.

```sh
cargo bench:engine
# Exercise every benchmark once without collecting timings:
cargo bench:engine --test
# Measure just one family, or save/compare a baseline:
cargo bench:engine query/top_k
cargo bench:engine --save-baseline before
cargo bench:engine --baseline before
```

Benchmark SQL lives in [`benches/queries`], embedded with `include_str!` so file I/O is excluded from timings.

The Criterion suite separates:

| Group                            | Work measured                                                       | Fixture size            |
| -------------------------------- | ------------------------------------------------------------------- | ----------------------- |
| `prepare`                        | Parsing and planning filters and aggregates                         | No filesystem traversal |
| `query`                          | Collected names, metadata, top-10 sorting, grouping, streamed names | 100 and 1,000 files     |
| `mutation/resolve_chmod`         | Planning, scanning, and freezing mutation targets                   | 10 and 100 files        |
| `mutation/apply_chmod_journaled` | Applying permissions and writing recovery records                   | 10 and 100 files        |

Fixtures use ten subdirectories, alternating `.rs`/`.txt` extensions, and deterministic file sizes. Query preparation and fixture creation are outside query timings. Apply benchmarks use a fresh fixture and resolved mutation per iteration; setup and fixture/journal cleanup are outside the timed section. Success checks are included in apply timings. No benchmark mutates an existing user directory.

Temporary fixtures live under `TMPDIR` (or the system temporary directory). Set `TMPDIR` to an existing directory on the filesystem you want to measure: journal durability costs on tmpfs are not representative of disk storage. These are warm-cache microbenchmarks; they do not flush the OS cache. Throughput counts fixture files, excluding their directories. Criterion writes reports below the Cargo target directory's `criterion` folder. Compare runs on the same machine, filesystem, and build configuration.

## Disposable filesystem runs (Linux)

Install Bubblewrap (`bwrap`) and use the opt-in Cargo configuration from the workspace root:

```sh
cargo sandbox:fsql --apply -F "${PWD}/examples/sandbox-mutations.fsql"

cargo sandbox:test
```

Cargo builds normally on the host. Each executable it launches through the runner gets a fresh writable `/tmp/fsql-sandbox` as its working directory. The host root is mounted read-only; `/tmp`, `/run`, `/dev`, and `/proc` are private mounts. Home and XDG paths point into the disposable filesystem, including fsql's default journal location. Network access is isolated. The runner fails if Bubblewrap or the required kernel namespace support is unavailable; it never falls back to running the command directly.

To start with existing files, set `FSQL_SANDBOX_SEED` to a directory. Its contents are copied into the writable tree, preserving symlinks; the source stays read-only. Relative seed paths are resolved against the invocation directory.

```sh
FSQL_SANDBOX_SEED=./tree-sitter-fsql/examples \
  cargo sandbox:fsql -F "${PWD}/examples/inventory.fsql"
```

Use paths under `/tmp/fsql-sandbox` for mutations. All sandbox files and journals disappear when that invocation ends; use multiple `-e` arguments or a query file to inspect mutations in the same run. Relative query files must exist in the copied tree; use an absolute host path for a read-only query file outside it.

This is a development guard against accidental host filesystem changes, not an environment for executing untrusted code: host files outside the private mounts remain readable, the process inherits environment variables and standard I/O, and compilation/build scripts run outside the sandbox. Filesystem characteristics are those of tmpfs, not necessarily the filesystem used in production.

Cargo wraps each test executable, not each test case. Subprocesses inherit its namespace. Directly launching a binary bypasses the Cargo configuration, and this configuration only selects Linux targets. Use it for native Linux runs; it does not supply cross-compilation emulators or load Zed's WASM extension.

Validate the wrapper itself with:

```sh
python3 scripts/check-sandbox.py
```

[Runner]: https://github.com/kjanat/runner
[`.cargo/config.toml`]: .cargo/config.toml
[`benches/queries`]: crates/fsql/benches/queries/
[examples]: examples/README.md
