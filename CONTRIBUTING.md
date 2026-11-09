# Development

The commands below call Cargo and Python directly. Contributors who use [Runner] can also invoke the repository's Cargo aliases and package scripts with `run fsql`, `run example:query crates`, `run bench:engine`, or `run sandbox:check`. Runner is optional. [`.cargo/config.toml`] defines the Cargo aliases.

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

Benchmark SQL lives in [`benches/queries`]. The benchmarks embed it with `include_str!`, and the timings exclude file I/O.

The Criterion suite includes these groups:

| Group                            | Work measured                                                       | Fixture size            |
| -------------------------------- | ------------------------------------------------------------------- | ----------------------- |
| `prepare`                        | Parsing and planning filters and aggregates                         | No filesystem traversal |
| `query`                          | Collected names, metadata, top-10 sorting, grouping, streamed names | 100 and 1,000 files     |
| `mutation/resolve_chmod`         | Planning, scanning, and freezing mutation targets                   | 10 and 100 files        |
| `mutation/apply_chmod_journaled` | Applying permissions and writing recovery records                   | 10 and 100 files        |

Fixtures use ten subdirectories, alternating `.rs`/`.txt` extensions, and deterministic file sizes. Query timings exclude query preparation and fixture creation. Each apply iteration uses a fresh fixture and resolved mutation. Apply timings exclude setup, fixture cleanup and journal cleanup, and include success checks. No benchmark mutates an existing user directory.

Temporary fixtures live under `TMPDIR`, or under the system temporary directory when `TMPDIR` is unset. Set `TMPDIR` to an existing directory on the filesystem you want to measure. Journal durability costs on tmpfs do not represent disk storage. The benchmarks run with a warm cache and do not flush the OS cache. Throughput counts fixture files and excludes directories. Criterion writes reports below the Cargo target directory's `criterion` folder. Compare runs on the same machine, filesystem, and build configuration.

## CLI comparisons and policy workloads

The [filesystem benchmark harness](benchmarks/README.md) compares complete commands with `find`, `fd`, `ls`, optional gahag/FSQL, and additional TOML adapters. It verifies equivalent results before and after timing, records executable hashes and raw samples, and supports multiple fsql versions or SQL variants on generated fixtures or existing trees.

```sh
cargo build:cli:locked
uv run bench
cargo bench:engine policy
cargo bench:engine pruning
cargo bench:engine relational
cargo bench:engine glob_sets
```

The Rust suite also embeds the original, scan-pruned, and indexed-join versions of the `hell` query from [`benchmarks/queries`]. See the harness documentation for comparison contracts, query templates, reproducibility limits, and baseline commands.

[`benchmarks/queries`]: benchmarks/queries/

## Disposable filesystem runs (Linux)

Install Bubblewrap, which provides `bwrap`, and use the opt-in Cargo configuration from the workspace root:

```sh
cargo sandbox:fsql --apply -F "${PWD}/examples/sandbox-mutations.fsql"
cargo sandbox:test
```

Cargo builds on the host, outside the sandbox. Each executable it launches through the runner gets a fresh writable `/tmp/fsql-sandbox` as its working directory. The runner mounts the host root read-only and gives the sandbox private `/tmp`, `/run`, `/dev`, and `/proc` mounts. Home and XDG paths point into the disposable filesystem, including fsql's default journal location. The sandbox has no network access. The runner fails if Bubblewrap or the required kernel namespace support is unavailable, and never runs the command directly.

To start with existing files, set `FSQL_SANDBOX_SEED` to a directory. The runner copies its contents into the writable tree and preserves symlinks. The source stays read-only. The runner resolves relative seed paths against the invocation directory.

```sh
export FSQL_SANDBOX_SEED="${PWD}/tree-sitter-fsql/examples"
cargo sandbox:fsql -F "${PWD}/examples/inventory.fsql"
```

Use paths under `/tmp/fsql-sandbox` for mutations. All sandbox files and journals disappear when that invocation ends; use multiple `-e` arguments or a query file to inspect mutations in the same run. Relative query files must exist in the copied tree; use an absolute host path for a read-only query file outside it.

This sandbox guards against accidental host filesystem changes during development. Do not use it to run untrusted code. Host files outside the private mounts remain readable, the process inherits environment variables and standard I/O, and compilation and build scripts run outside the sandbox. Filesystem characteristics are those of tmpfs and may differ from the production filesystem.

Cargo wraps each test executable. Test cases within one executable share the sandbox, and subprocesses inherit its namespace. Launching a binary directly bypasses the Cargo configuration, and the configuration selects only Linux targets. Use it for native Linux runs. It supplies no cross-compilation emulators and does not load Zed's WASM extension.

Validate the wrapper itself with:

```sh
python3 scripts/check-sandbox.py
```

[Runner]: https://github.com/kjanat/runner
[`.cargo/config.toml`]: .cargo/config.toml
[`benches/queries`]: crates/fsql/benches/queries/
[examples]: examples/README.md
