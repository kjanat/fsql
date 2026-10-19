# Filesystem benchmarks

There are three complementary suites:

- `uv run bench` measures complete CLI invocations, including startup and output generation, against equivalent filesystem tools.
- `cargo bench:engine` measures the Rust library with Criterion, excluding process startup, SQL preparation, fixture setup, and output formatting from execution timings. The existing `prepare` group measures preparation separately.

- `cargo bench:traversal` compares Rust filesystem walkers directly, including path collection and worker startup.

Build the CLI before running the command suite:

```sh
cargo build --release --package fsql-cli --locked
uv run bench
```

The default creates a disposable deterministic tree with hidden files, ignored directories, sparse files, symlinks, and filenames containing spaces, quotes, and Unicode. It compares `fsql`, GNU `find`, `fd`, GNU `ls`, `bfs`, and `fselect` where their semantics overlap. Missing optional tools and unsupported cases are reported as skips. The harness uses only Python’s standard library. `uv run bench` uses the project’s pinned Python; direct execution with `python3 benchmarks/run.py` also works with Python 3.11 or newer. Runner users can use `run bench:cli` and `run bench:check`.

`bfs` uses find-compatible predicates and NUL output. `fselect` uses its NUL-separated `into list` output and the checked-in [`fselect.toml`](fselect.toml), which disables colors, ignore rules, update checks, and indexed search backends. CI and local development source the tools from [`.tool-versions`](../.tool-versions) with mise. Run `mise install`, then `mise exec -- uv run bench` to use those tools without depending on shell activation. CI uses `uv run python` for harness tests, respecting the project’s `.python-version`.

## Real trees and binaries

```sh
uv run bench --root /path/to/project --runs 10 --output /tmp/project-bench.json

# Compare two existing binaries; the harness never checks out, builds, or installs them.
uv run bench \
  --fsql current=./target/release/fsql \
  --fsql-legacy before=/path/to/pre-pr8/fsql \
  --root /path/to/project --output /tmp/before-after.json

# Disable external tools, select cases, or control fd's parallelism.
uv run bench --tools --case paths --case exclude
uv run bench --tools find fd --fd-threads 1

# Optional, already-built gahag/FSQL executable.
uv run bench --gahag /path/to/gahag/fsql
```

`--fsql LABEL=PATH` can be repeated for versions that support the current dialect. `--fsql-legacy` uses a regular expression instead of `relative_path GLOB ANY` for the exclusion case, so pre-PR8 binaries can participate. This compares equivalent results using the capabilities available in each version; it is not an identical-query microbenchmark. Use `--query` for identical SQL across binaries.

The existing root is only read during verification and timing. Put the report outside the measured tree. For `--root /`, where no outside destination exists, the report is published only after the final input snapshot and executable checks; its directory and temporary output file are created at that point. Any previous report remains part of the measured tree. Default reports go to `target/benchmarks/latest.json`; the harness rejects that destination if a non-filesystem-root `--root` contains it. The generated fixture is removed after the run. Reports include raw samples, medians/ranges, exact argument arrays and stdin, output codecs, binary SHA-256 hashes, harness hash, platform, thread settings, seed, and skipped cases. Use concrete executables rather than version-manager shims or wrapper scripts, whose hashes identify the launcher rather than its eventual target. Label binaries with meaningful revisions; a hash identifies the executable independently of a checkout's current revision.

## Shared semantics

| Case      | Contract                                                                                     | Tools                                   |
| --------- | -------------------------------------------------------------------------------------------- | --------------------------------------- |
| `paths`   | Every descendant entry, excluding the root                                                   | fsql, find, fd, bfs, fselect, gahag     |
| `shallow` | Immediate children only                                                                      | fsql, find, fd, ls, bfs, fselect, gahag |
| `size`    | Regular files strictly larger than `--size` bytes, default 1,000,000                         | fsql, find, fd, bfs, fselect            |
| `suffix`  | Entry paths ending in case-sensitive `.rs`                                                   | fsql, find, fd, bfs, fselect, gahag     |
| `exclude` | Every descendant except entries with `.git`, `target`, or `node_modules` as a path component | fsql, find, fd, bfs, fselect, gahag     |

Commands use `LC_ALL=C.UTF-8`. Hidden and ignored entries are included. Symlinks are listed but never followed. The size case excludes symlinks and directories, because the tools do not all expose identical size predicates for those entry types. `fd` gets `--hidden --no-ignore --case-sensitive`; it retains its normal thread count unless overridden. Shallow scans use explicit depth limits in fsql, find, and fd. `ls` is used only for unsorted shallow listing, without parsing recursive headings or inventing filtering it cannot perform.

The exclusion adapter uses separate directory and descendant glob patterns. Do not substitute `{,/**}` without checking results: current empty-alternative glob behavior can retain the directory itself. `find`, `fd`, and `bfs` can prune excluded trees too. `fselect` uses a result filter for this case.

Every supported command must match an independent Python `scandir`/`lstat` oracle before **and after** timing. Comparison preserves duplicate counts and ignores order. Newlines, tabs, quotes, and Unicode are preserved through JSON output for fsql and NUL output for find/fd/ls/bfs/fselect. gahag's REPL framing is decoded separately; that adapter is skipped if filenames contain CR or LF. Non-UTF-8 filenames and `.fsql-*` staging names are explicitly rejected rather than silently normalized into a false comparison.

Reports are written to a temporary file beside the destination, flushed, and atomically replaced after a successful write. A failed write or replacement preserves the previous report.

Each subprocess must exit successfully with empty stderr, including timed runs. Timeouts, wrong rows, duplicates, unexpected diagnostics, changed input metadata, or a changed executable fail the run without publishing a new success report. This detects many concurrent changes but is not an atomic filesystem snapshot; use a quiet tree. Reads may update access times, which are intentionally excluded from stability checks.

One warmup and seven measured runs are the defaults. Command order is shuffled each round with a recorded deterministic seed. Timing includes process startup and serialization to `/dev/null`; correctness decoding is outside timing. These are **CLI workload comparisons**, so JSON serialization versus NUL output is part of their cost. These are warm-cache runs: the oracle itself visits the tree, no caches are flushed, and no cold-disk claims are made. Avoid concurrent compilation, indexing, or unrelated workloads when collecting publishable timings.

## Extend the command suite

Additional tools can participate through a trusted TOML adapter without changing the harness. See [`adapters/find-regex.toml`](adapters/find-regex.toml):

```sh
uv run bench --adapter benchmarks/adapters/find-regex.toml --case suffix
```

An adapter has a unique `name` and `[cases.CASE]` sections containing an `argv` array and a `codec` (`nul` or `json`). Executables are resolved before changing directory; commands run with the input tree as their working directory. Argument substitutions are `{{root}}`, `{{size}}`, and `{{size_plus_one}}`. Arguments are passed directly, without a shell. NUL output may contain relative or root-contained absolute paths; JSON must contain one text field named `path` with an absolute path. Custom adapters are executable code: only run ones you trust. They must be read-only. They undergo the same independent result checks as built-in tools.

Adding a genuinely different workload requires a new case contract, oracle predicate, adapters that can satisfy it, and a contract test. Unsupported tools should skip the case rather than quietly measure different work.

## SQL workloads and making `hell` faster

The original policy/recursive workload is now checked in as a root-parameterized query. It remains a fixed illustrative policy, not an implementation of Git ignore/index semantics. Compare four equivalent versions:

```sh
uv run bench --root /path/to/project \
  --query hell=benchmarks/queries/hell.fsql \
  --query pruned=benchmarks/queries/hell-pruned.fsql \
  --query joined=benchmarks/queries/hell-joined.fsql \
  --query indexed=benchmarks/queries/hell-indexed.fsql \
  --ordered --output /tmp/hell.json
```

With no `--root`, the same command uses the generated fixture. `--query` replaces the built-in cases and external-tool comparisons. Templates use `{{root}}` as a complete SQL string literal; do not add quotes. Each query must return one absolute text column named `path`. The first query/binary supplies the expected result, so this mode proves equivalence, not correctness against an independent policy interpreter. `--ordered` additionally verifies identical output order. Use the original template on both old and new binaries to compare the exact same query.

The variants isolate three improvements:

1. **Scan pruning:** project the native `relative_path` and move mandatory exclusions into the inventory's `files(...)` query. The original materializes the full inventory and only then filters the next CTE.
2. **Indexed final join:** rename the subtree grouping key to `parent` so the final left join can use `USING (parent)`. The executor currently indexes `USING` joins; equivalent `ON a.key = b.key` joins use nested loops.
3. **Indexed rule lookup:** replace the correlated higher-priority lookup with a `USING (path, policy_group)` join to grouped maxima, preserving deny-on-tie behavior. The `joined` variant retains the correlated lookup so its cost can be measured separately.

These are SQL rewrites, not engine changes. The original remains a regression baseline. Future engine work can target predicate pushdown across safe CTE boundaries, indexing equality conditions in `ON`, and decorrelating eligible `EXISTS` predicates. Preserve NULL behavior, duplicate multiplicity, outer joins, rule precedence, and output ordering when doing so. The rule-pattern join still needs pattern evaluation, and recursive ancestry/grouping still has a cost even after these rewrites.

## Rust benchmarks

```sh
cargo bench:engine --test                       # run every correctness assertion once
cargo bench:engine policy                       # original / pruned / joined / indexed hell
cargo bench:engine pruning                      # identical results, with/without traversal pruning
cargo bench:engine relational                   # ON nested loop versus USING index
cargo bench:engine glob_sets                    # scalar OR versus constant GLOB ANY
cargo bench:engine --save-baseline before
cargo bench:engine --baseline before
```

The new groups extend the existing preparation, scan, metadata, top-k, aggregation, streaming, and journaled mutation benchmarks:

- `policy`: shared SQL templates with 100/1,000 ordinary files, four times as many excluded build artifacts, and explicit allow/deny cases. Values and ordering must match across all variants.
- `pruning`: the same filesystem fixture and predicate, with `IS TRUE` preventing optimizer extraction in the row-filter baseline. Result sets must match.
- `relational`: 100/1,000 in-memory `VALUES` rows on each side, isolating equality-join execution without filesystem or output costs.
- `glob_sets`: 100/1,000 in-memory paths, comparing scalar glob disjunctions and constant quantified glob sets.

Criterion includes repeated execution and its per-execution caches; queries are prepared outside the timed iteration. Fixture creation and correctness checks are also outside timings. Reports live under `target/criterion`. `TMPDIR` controls the filesystem used for generated fixtures. Do not interpret tmpfs timings as disk durability costs.

## Traversal library comparisons

```sh
cargo bench:traversal --test                 # validate all 96 comparisons
cargo bench:traversal 'traversal/wide/Paths' # measure one workload across walkers
cargo bench:traversal --save-baseline before
cargo bench:traversal --baseline before
```

The suite compares an explicit-stack `std::fs::read_dir` walker, `walkdir`, serial and four-worker `ignore`, serial and four-worker `jwalk`, direct `tree-fucker::Scan`, and `fsql::walk::Walker`. These are benchmark-only dependencies. `run bench:traversal` resolves the same Cargo alias.

Each generated tree contains 300 ordinary files: one file per directory (wide), up to 32 nested levels (deep), or 80% of files under `target` (excluded). Fixtures also contain hidden entries, a newline in a filename, excluded directories and files, a directory symlink, and a broken symlink. Setup is outside timing; the host governor is installed once before measurements.

Four workloads run on each tree:

- `Paths`: collect all descendant paths, excluding the root.
- `Metadata`: use `symlink_metadata` uniformly to collect regular files strictly larger than 100 bytes. This isolates traversal plus a common metadata workload; it does not measure each library's metadata-cache API.
- `Exclude`: omit `.git`, `target`, and `node_modules` components, pruning descent where supported. The public Walker exposes no pruning control, so its case is explicitly named `fsql-walker-row-filter`. Engine-level SQL pruning is measured separately by `bench:engine pruning`.
- `FirstTen`: stop after ten distinct descendant paths. Different traversal orders may select different entries; validation checks count, uniqueness, and membership rather than identical ordering.

All full-result comparisons are checked against an unpruned standard-library inventory before timing, preserving duplicate counts. Ignore rules are disabled, symlinks are never followed, and errors fail the benchmark. Output collection is included, sorting and correctness checks are excluded. Parallel `ignore` workers collect locally and merge once per worker; early stopping uses a shared counter. Parallel `jwalk` creates a fresh four-worker pool per invocation and joins every worker before returning (including after early stop), and `ignore` creates its workers per invocation too. Thus these timings include worker startup and early-stop scheduling costs rather than assuming a persistent application pool.

Direct `Scan` uses its default anchor budget (zero); Walker uses its own anchor policy. Both use the same installed governor and allow filesystem crossings. The raw Scan and Walker cases therefore measure their respective public configurations, not identical internal options. These fixtures measure warm-cache local traversal; they do not establish universal rankings or cold-storage behavior.

## Validation

```sh
python3 -m unittest discover -s benchmarks -p 'test_*.py'
uv run bench --files 100 --warmups 0 --runs 1
cargo bench:engine --test
cargo bench:traversal --test
```

CI runs these correctness checks without performance thresholds. The Python integration tests cover real adapters, exact size boundaries, symlinks, special filenames, duplicate detection, stderr failures, and same-size input changes. Optional external tools are tested when installed. Timings from shared CI runners are not performance acceptance criteria.

## Layer attribution and profiling

Use [the layer probes](layers.md) to separate raw Scan, Walker, SQL, output and allocation costs. `cargo bench:layers --fixture 20000 all` runs timings; `cargo bench:alloc --fixture 20000 all` measures allocations separately. Both have CI smoke coverage.
