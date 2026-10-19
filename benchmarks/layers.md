# Layer benchmarks and profiles

Run from the repository root. These are release-mode in-process probes, separate from the CLI and Criterion benchmarks. Each repository builds against its own dependency graph: fsql measures its pinned tree-fucker revision; tree-fucker measures its local library. No sibling checkout or temporary source package is required.

```sh
cargo bench:layers --test
cargo bench:alloc --test
cargo bench:layers --fixture 20000 all 15 2
cargo bench:alloc --fixture 20000 all 3 1
cargo bench:layers /absolute/path/to/input scan-count 25 2
```

The arguments are `ROOT MODE [RUNS=15] [WARMUPS=2]`, or `--fixture FILES MODE [RUNS] [WARMUPS]`. `MODE=all` runs the repository's complete layer suite sequentially. `--help` lists modes. Zero warmups are allowed; measured runs and fixture files must be positive. Input trees must remain unchanged throughout measurement.

Generated fixtures live under **target/benchmarks**, and are removed when the probe exits. They contain ten directories of sparse files, a hidden directory, Unicode/quoted names and, on Unix, a directory symlink and dangling symlink. Symlinks are not followed. One in seven regular files exceeds 1,000,000 bytes. For comparisons between repositories, pass the same existing input tree to both probes. Generated fixtures are not the same shape as the Python CLI benchmark fixture.

| Mode         | Work measured                                                              |
| ------------ | -------------------------------------------------------------------------- |
| std-count    | std::fs directory traversal and entry count, without collecting file paths |
| scan-count   | raw Scan event traversal, no absolute file path materialization            |
| scan-anchors | same Scan with an anchor budget of 1024 instead of zero                    |
| scan-paths   | Scan plus absolute path materialization per entry                          |
| scan-stats   | Scan plus diagnostic ScanStats output; use for diagnostics, not timings    |

The governor is installed once with foreground duty 1.0. Scan follows filesystem domains, uses an unlimited time ceiling and fails on errors, unlisted directories or unexpected boundaries. Anchors retain their safety semantics; this is a bounded-budget comparison, not a proposal to remove them.

Before each mode, an independent std::fs walk supplies the expected **complete path multiset**, which is compared with the probe's results. Verification materializes paths outside measured iterations. Each iteration checks its row count; the oracle is repeated afterward to detect changed membership. This does not detect every possible concurrent metadata change. Root entries are excluded. Verification checks query paths, not round-tripping the serialized output. The existing CLI benchmark remains responsible for external tool/output equivalence.

TSV results go to stdout; root, run settings and ScanStats go to stderr. Save reports outside the input tree, for example:

```sh
mkdir -p target/benchmarks
cargo bench:layers /absolute/path/to/input all 25 2 > target/benchmarks/layers.tsv
cargo bench:alloc /absolute/path/to/input all 3 1 > target/benchmarks/allocations.tsv
```

Timing uses the normal system allocator. **The allocation executable is separate** and uses stats_alloc only as a development dependency; no unsafe code is added to either library. Its timing columns include instrumentation overhead and must not be compared with the timing executable. Allocation columns are per measured iteration (integer averages), separating allocations and reallocations. `bytes_allocated` follows stats_alloc's definition, including positive reallocation growth; it is not RSS, peak memory, or the sum of full requested realloc sizes. Normal timing runs have zero allocation counters because their allocator is not instrumented. Preparation and teardown inside the measured operation are included; fixture creation, oracle verification and governor installation are excluded.

Modes run sequentially with warm caches; there is no randomization, cold-cache guarantee or performance threshold. Do not subtract independently measured medians as an exact attribution of cost. CI runs both executables with `--test` as correctness smoke checks, not performance gates.

## Profiling

Build first, then invoke the executable directly to avoid profiling Cargo. The normal executable is `target/release/examples/layers`; the instrumented one is `target/release/examples/layers_alloc`. For symbolized profiles, build with `CARGO_PROFILE_RELEASE_DEBUG=1` (and `--package fsql` in fsql):

```sh
cargo build --release --example layers --locked
mkdir -p target/benchmarks
set -o pipefail
perf record -o target/benchmarks/layers.perf -- target/release/examples/layers /absolute/path/to/input scan-count 100 2 2>&1 | envctl redact
perf report --stdio -i target/benchmarks/layers.perf 2>&1 | envctl redact
strace -c target/release/examples/layers /absolute/path/to/input scan-count 1 0 2>&1 | envctl redact
```

Profiles and syscall totals include startup, the pre/post oracle walks, one verification traversal, warmups and measured traversals. Use enough measured iterations for CPU profiles to be dominated by the workload; **do not divide total syscall counts by RUNS and call that a single traversal**. Allocation regions exclude verification and warmups. Keep traced/instrumented results separate from normal timing. `perf`, `strace` and `envctl` are optional profiling tools, not prerequisites for the benchmarks.

## fsql layers

| Mode             | Additional work                                                        |
| ---------------- | ---------------------------------------------------------------------- |
| walker-count     | fsql Walker entries, counted without collecting paths                  |
| query-count      | prepare and stream the path query, no serialization                    |
| query-lines      | path query with buffered Lines output                                  |
| query-json       | path query with buffered existing JSON output                          |
| query-flush      | existing JSON writer flushed per row, matching CLI behavior            |
| query-direct     | experimental single-text-column JSON writer; not a general replacement |
| query-size-count | regular-file size predicate, no serialization                          |
| query-size-json  | regular-file size predicate and buffered JSON output                   |

Output goes to `/dev/null`. Queries select `path` with `depth > 0`; size queries also require `kind = 'file' AND size > 1000000`. Query preparation is included in each measured operation. These are in-process measurements, so use `run bench` to include CLI startup and compare external tools. Use `cargo bench:traversal` for vector-collecting adapters and the other Rust walker libraries, and `cargo bench:engine` for Criterion engine microbenchmarks.

The [original investigation](investigations/2026-10-19-layers.md) records the historical baseline and its different methodology. New measurements should use these checked-in probes.
