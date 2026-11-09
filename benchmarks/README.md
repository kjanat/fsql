# Filesystem benchmarks

The benchmarks have three suites:

- `uv run bench` measures complete CLI invocations, including startup and output generation, against equivalent filesystem tools.
- `cargo bench:engine` measures the Rust library with Criterion. Execution timings exclude process startup, SQL preparation, fixture setup, and output formatting. The `prepare` group measures preparation separately.
- `cargo bench:traversal` compares Rust filesystem walkers directly, including path collection and worker startup.

Build the CLI before running the command suite:

```sh
cargo build --release --package fsql-cli --locked
uv run bench
```

By default the harness creates a disposable deterministic tree with hidden files, ignored directories, sparse files, symlinks, and filenames that contain spaces, quotes, and Unicode. It compares `fsql`, GNU `find`, `fd`, GNU `ls`, `bfs`, and `fselect` where their semantics overlap. The harness reports missing optional tools and unsupported cases as skips. The harness uses only Python's standard library. `uv run bench` uses the project's pinned Python; direct execution with `python3 benchmarks/run.py` also works with Python 3.11 or newer. Runner users can use `run bench:cli` and `run bench:check`.

`bfs` uses find-compatible predicates and NUL output. `fselect` uses its NUL-separated `into list` output and the checked-in [`fselect.toml`](fselect.toml), which disables colors, ignore rules, update checks, and indexed search backends. CI and local development source the tools from [`.tool-versions`](../.tool-versions) with mise. The rolling selectors in `.tool-versions` keep CI on current tools, so leave them unpinned. Reports identify the actual executables by SHA-256, and correctness checks reject incompatible results before timing. To use those tools without shell activation, run `mise install`, then `mise exec -- uv run bench`. CI runs harness tests with `uv run python`, which reads the project's `.python-version`.

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

Repeat `--fsql LABEL=PATH` for each version that supports the current dialect. `--fsql-legacy` uses a regular expression instead of `relative_path GLOB ANY` for the exclusion case, so pre-PR8 binaries can participate. These comparisons check for equivalent results, and each version runs the SQL its own features allow. Use `--query` for identical SQL across binaries.

During verification and timing, the harness only reads the existing root. Put the report outside the measured tree. For `--root /`, where no outside destination exists, the harness publishes the report only after the final input snapshot and executable checks, and creates the report directory and temporary output file at that point. Any previous report remains part of the measured tree. Default reports go to `target/benchmarks/latest.json`; the harness rejects that destination if a non-filesystem-root `--root` contains it. The harness removes the generated fixture after the run. Reports include raw samples, medians and ranges, exact argument arrays and stdin, output codecs, binary SHA-256 hashes, the harness hash from before execution, platform, thread settings, seed, and skipped cases. Pass concrete executables. For a version-manager shim or wrapper script, the hash identifies the launcher and says nothing about the program it starts. Label binaries with meaningful revisions; a hash identifies the executable independently of a checkout's current revision.

## Shared semantics

| Case      | Contract                                                                                     | Tools                                   |
| --------- | -------------------------------------------------------------------------------------------- | --------------------------------------- |
| `paths`   | Every descendant entry, excluding the root                                                   | fsql, find, fd, bfs, fselect, gahag     |
| `shallow` | Immediate children only                                                                      | fsql, find, fd, ls, bfs, fselect, gahag |
| `size`    | Regular files strictly larger than `--size` bytes, default 1,000,000                         | fsql, find, fd, bfs, fselect            |
| `suffix`  | Entry paths ending in case-sensitive `.rs`                                                   | fsql, find, fd, bfs, fselect, gahag     |
| `exclude` | Every descendant except entries with `.git`, `target`, or `node_modules` as a path component | fsql, find, fd, bfs, fselect, gahag     |

Commands use `LC_ALL=C.UTF-8`. Every command includes hidden and ignored entries and lists symlinks without following them. The size case excludes symlinks and directories, since the tools expose different size predicates for those entry types. `fd` gets `--hidden --no-ignore --case-sensitive` and keeps its normal thread count unless overridden. Shallow scans use explicit depth limits in fsql, find, and fd. The harness uses `ls` only for unsorted shallow listing. It does not parse recursive `ls` headings or emulate filters that `ls` lacks.

The exclusion adapter uses separate directory and descendant glob patterns. Check results before substituting `{,/**}`. Current empty-alternative glob behavior can retain the directory itself. `find`, `fd`, and `bfs` can prune excluded trees too. `fselect` uses a result filter for this case.

The harness rechecks its own source hash before it publishes a report, and discards the results if the source changed during the run. Ordinary names such as `.fsql-notes` remain valid benchmark inputs.

Every supported command must match an independent Python `scandir` and `lstat` oracle before and after timing. Comparison preserves duplicate counts and ignores order. Newlines, tabs, quotes, and Unicode survive JSON output for fsql and NUL output for find, fd, ls, bfs and fselect. The harness decodes gahag's REPL framing separately and skips that adapter if filenames contain CR or LF. The harness rejects non-UTF-8 filenames and reserved `.fsql-<hex>-<hex>-<decimal>` staging names.

The harness writes each report to a temporary file beside the destination, flushes it, and atomically replaces the destination after a successful write. A failed write or replacement preserves the previous report.

Each subprocess must exit successfully with empty stderr, including timed runs. Timeouts, wrong rows, duplicates, unexpected diagnostics, changed input metadata, or a changed executable fail the run without publishing a new success report. These checks detect many but not all concurrent changes, so use a quiet tree. Reads can update access times, so the stability checks ignore them.

The defaults are one warmup and seven measured runs. The harness shuffles command order each round with a recorded deterministic seed. Timing includes process startup and serialization to `/dev/null`, and excludes correctness decoding. These are CLI workload comparisons, so the cost of JSON serialization or NUL output is part of each result. Runs are warm-cache. The oracle visits the tree and the harness flushes no caches, so the results say nothing about cold disks. Avoid concurrent compilation, indexing, or unrelated workloads when collecting publishable timings.

## Extend the command suite

Additional tools can participate through a trusted TOML adapter without changing the harness. See [`adapters/find-regex.toml`](adapters/find-regex.toml):

```sh
uv run bench --adapter benchmarks/adapters/find-regex.toml --case suffix
```

An adapter has a unique `name` and `[cases.CASE]` sections. Each section holds an `argv` array and a `codec`, either `nul` or `json`. The harness resolves executables before it changes directory, and each command runs with the input tree as its working directory. Argument substitutions are `{{root}}`, `{{size}}`, and `{{size_plus_one}}`. The harness passes arguments directly, without a shell. NUL output may contain relative or root-contained absolute paths; JSON must contain one text field named `path` with an absolute path. Custom adapters are executable code, so run only adapters you trust. They must be read-only. The harness applies the same independent result checks to them as to built-in tools.

A different workload needs a new case contract, an oracle predicate, adapters that can satisfy it, and a contract test. A tool that cannot satisfy a case should skip it.

## SQL workloads and making `hell` faster

[`queries/hell.fsql`](queries/hell.fsql) holds the policy/recursive workload as a root-parameterized query. It is a fixed illustrative policy and does not implement Git ignore or index semantics. Compare four equivalent versions:

```sh
uv run bench --root /path/to/project \
  --query hell=benchmarks/queries/hell.fsql \
  --query pruned=benchmarks/queries/hell-pruned.fsql \
  --query joined=benchmarks/queries/hell-joined.fsql \
  --query indexed=benchmarks/queries/hell-indexed.fsql \
  --ordered --output /tmp/hell.json
```

With no `--root`, the same command uses the generated fixture. `--query` replaces the built-in cases and external-tool comparisons. Templates use `{{root}}` as a complete SQL string literal; do not add quotes. Each query must return one absolute text column named `path`. The first query or binary supplies the expected result, so this mode proves equivalence between variants only. `--ordered` also checks that the output order is identical. Use the original template on both old and new binaries to compare the exact same query.

The variants isolate three improvements:

1. **Scan pruning.** Project the native `relative_path` and move mandatory exclusions into the inventory's `files(...)` query. The original materializes the full inventory and only then filters the next CTE.
2. **Indexed final join.** Rename the subtree grouping key to `parent` so the final left join can use `USING (parent)`. The executor currently indexes `USING` joins; equivalent `ON a.key = b.key` joins use nested loops.
3. **Indexed rule lookup.** Replace the correlated higher-priority lookup with a `USING (path, policy_group)` join to grouped maxima. The join preserves deny-on-tie behavior. The `joined` variant keeps the correlated lookup.

The variants are SQL rewrites that need no engine change. The original query is the regression baseline. Possible engine work includes predicate pushdown across safe CTE boundaries, indexing equality conditions in `ON`, and decorrelating eligible `EXISTS` predicates. Each such change must preserve NULL behavior, duplicate multiplicity, outer joins, rule precedence, and output ordering. The rule-pattern join still evaluates patterns, and recursive ancestry and grouping still cost time after these rewrites.

## Rust benchmarks

```sh
cargo bench:engine --test     # run every correctness assertion once
cargo bench:engine policy     # original / pruned / joined / indexed hell
cargo bench:engine pruning    # identical results, with/without traversal pruning
cargo bench:engine relational # ON nested loop versus USING index
cargo bench:engine glob_sets  # scalar OR versus constant GLOB ANY
cargo bench:engine --save-baseline before
cargo bench:engine --baseline before
```

Besides the preparation, scan, metadata, top-k, aggregation, streaming, and journaled mutation benchmarks, the engine suite has these groups:

- `policy` runs shared SQL templates over 100 or 1,000 ordinary files, four times as many excluded build artifacts, and explicit allow and deny cases. Values and ordering must match across all variants.
- `pruning` runs one filesystem fixture and predicate with and without pruning. The row-filter baseline uses `IS TRUE` to block optimizer extraction. Result sets must match.
- `relational` joins 100 or 1,000 in-memory `VALUES` rows on each side. The equality join runs with no filesystem or output costs.
- `glob_sets` matches 100 or 1,000 in-memory paths and compares scalar glob disjunctions against constant quantified glob sets.

Criterion timings include repeated execution and its per-execution caches. The benchmarks prepare queries outside the timed iteration, and fixture creation and correctness checks also fall outside timings. Reports live under `target/criterion`. `TMPDIR` selects the filesystem for generated fixtures. tmpfs timings say nothing about disk durability costs.

## Traversal library comparisons

```sh
cargo bench:traversal --test                 # validate all 96 comparisons
cargo bench:traversal 'traversal/wide/Paths' # measure one workload across walkers
cargo bench:traversal --save-baseline before
cargo bench:traversal --baseline before
```

The suite compares an explicit-stack `std::fs::read_dir` walker, `walkdir`, serial and four-worker `ignore`, serial and four-worker `jwalk`, direct `tree-fucker::Scan`, and `fsql::walk::Walker`. These are benchmark-only dependencies. `run bench:traversal` resolves the same Cargo alias.

Each generated tree contains 300 ordinary files. The `wide` tree has one file per directory, the `deep` tree nests files up to 32 levels, and the `excluded` tree puts 80% of its files under `target`. Fixtures also contain hidden entries, a newline in a filename, excluded directories and files, a directory symlink, and a broken symlink. Setup is outside timing, and the benchmark installs the host governor once before measurements.

Four workloads run on each tree:

- `Paths` collects all descendant paths except the root.
- `Metadata` calls `symlink_metadata` on every entry to collect regular files strictly larger than 100 bytes. This measures traversal plus one common metadata workload and bypasses each library's metadata-cache API.
- `Exclude` omits `.git`, `target`, and `node_modules` components and prunes descent where the walker supports it. The public Walker has no pruning control, so its case is named `fsql-walker-row-filter`. `bench:engine pruning` measures engine-level SQL pruning.
- `FirstTen` stops after ten distinct descendant paths. Different traversal orders may select different entries; validation checks count, uniqueness, and membership.

The harness checks all full-result comparisons against an unpruned standard-library inventory before timing and preserves duplicate counts. The walkers run with ignore rules disabled and never follow symlinks, and any error fails the benchmark. Timings include output collection and exclude sorting and correctness checks. Parallel `ignore` workers collect locally and merge once per worker, and early stopping uses a shared counter. Parallel `jwalk` creates a fresh four-worker pool per invocation and joins every worker before returning, including after early stop. `ignore` also creates its workers per invocation. These timings therefore include worker startup and early-stop scheduling costs.

Direct `Scan` uses its default anchor budget of zero, and Walker uses its own anchor policy. Both use the same installed governor and allow filesystem crossings. The raw Scan and Walker cases therefore measure their respective public configurations. These fixtures measure warm-cache local traversal only.

## Validation

```sh
python3 -m unittest discover -s benchmarks -p 'test_*.py'
uv run bench --files 100 --warmups 0 --runs 1
cargo bench:engine --test
cargo bench:traversal --test
```

CI runs these correctness checks without performance thresholds. The Python integration tests cover real adapters, exact size boundaries, symlinks, special filenames, duplicate detection, stderr failures, and same-size input changes. The tests cover optional external tools when they are installed. Do not use timings from shared CI runners as performance acceptance criteria.

## Layer attribution and profiling

Use [the layer probes](layers.md) to separate raw Scan, Walker, SQL, output and allocation costs. `cargo bench:layers --fixture 20000 all` runs timings; `cargo bench:alloc --fixture 20000 all` measures allocations separately. Both have CI smoke coverage.
