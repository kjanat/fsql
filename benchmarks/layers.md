# Layer benchmarks and profiles

Run from the repository root. These are Cargo `[[bench]]` targets with `harness = false`. They use the optimized bench profile and a custom in-process measurement harness, and they complement the CLI and Criterion benchmarks. fsql measures crates.io `tree-fucker` 0.1.0, and the tree-fucker repository measures its local library. Neither repository needs a sibling checkout or a temporary source package.

```sh
cargo bench:layers --test
cargo bench:alloc --test
cargo bench:layers --fixture 20000 all 15 2
cargo bench:alloc --fixture 20000 all 3 1
cargo bench:layers /absolute/path/to/input scan-count 25 2
```

The aliases invoke `cargo bench --package fsql --bench layers --` and `cargo bench --package fsql --bench layers_alloc --`. The entry points and shared harness live under `benches/`. The custom argument parser accepts Cargo's injected `--bench` flag.

The arguments are `ROOT MODE [RUNS=15] [WARMUPS=2]`, or `--fixture FILES MODE [RUNS] [WARMUPS]`. `MODE=all` runs the repository's complete layer suite sequentially. `--help` lists modes. `WARMUPS` may be zero; `RUNS` and `FILES` must be positive. Input trees must remain unchanged throughout measurement.

The probe generates fixtures under `target/benchmarks` and removes them on exit. They contain ten directories of sparse files, a hidden directory, names with Unicode and quotes, and, on Unix, a directory symlink and a dangling symlink. The probes do not follow symlinks. One in seven regular files exceeds 1,000,000 bytes. For comparisons between repositories, pass the same existing input tree to both probes. The generated fixtures differ in shape from the Python CLI benchmark fixture.

| Mode         | Work measured                                                              |
| ------------ | -------------------------------------------------------------------------- |
| std-count    | std::fs directory traversal and entry count, without collecting file paths |
| scan-count   | raw Scan event traversal, no absolute file path materialization            |
| scan-anchors | same Scan with an anchor budget of 1024 instead of zero                    |
| scan-paths   | Scan plus absolute path materialization per entry                          |
| scan-stats   | Scan plus diagnostic ScanStats output; use for diagnostics only            |

The probe installs the governor once with foreground duty 1.0. Scan follows filesystem domains and uses an unlimited time ceiling. It fails on errors, unlisted directories, and unexpected boundaries. Anchors keep their safety semantics under the bounded budget.

Before each mode, an independent std::fs walk supplies the expected complete path multiset, and the probe compares it with its results. Verification materializes paths outside measured iterations. Each iteration checks its row count, and the probe repeats the oracle afterward to detect changed membership. This misses some concurrent metadata changes. The expected multiset excludes root entries. Verification checks query paths and skips round-tripping the serialized output. The CLI benchmark checks external tool and output equivalence.

TSV results go to stdout; root, run settings and ScanStats go to stderr. Save reports outside the input tree, for example:

```sh
mkdir -p target/benchmarks
cargo bench:layers /absolute/path/to/input all 25 2 >target/benchmarks/layers.tsv
cargo bench:alloc /absolute/path/to/input all 3 1 >target/benchmarks/allocations.tsv
```

Timing uses the normal system allocator. The allocation executable is separate and uses stats_alloc only as a development dependency, so neither library gains unsafe code. Its timing columns include instrumentation overhead, so do not compare them with the timing executable. Allocation columns hold integer averages per measured iteration and count allocations and reallocations separately. `bytes_allocated` follows stats_alloc's definition, including positive reallocation growth. It differs from RSS, peak memory, and the sum of full requested realloc sizes. Normal timing runs use an uninstrumented allocator and report zero allocation counters. The measured operation includes preparation and teardown and excludes fixture creation, oracle verification, and governor installation.

Modes run sequentially with warm caches, without randomization or performance thresholds. Subtracting independently measured medians gives only an approximate cost attribution. CI runs both executables with `--test` as correctness smoke checks.

## Profiling

Build first, then invoke the executable directly to avoid profiling Cargo. Benchmark executable paths depend on Cargo's toolchain and build layout, so read the exact path from the JSON artifact output. For symbolized profiles, set `CARGO_PROFILE_BENCH_DEBUG=1` on the build command below. Use `--bench layers_alloc` to build the separate allocation executable.

```sh
set -o pipefail
layers_bin=$(cargo bench --package fsql --bench layers --no-run --locked --message-format=json \
  | python3 -c 'import json, sys; artifacts = [json.loads(line) for line in sys.stdin]; paths = [a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("target", {}).get("name") == "layers" and a.get("executable")]; assert len(paths) == 1, paths; print(paths[0])')
mkdir -p target/benchmarks
perf record -o target/benchmarks/layers.perf -- "$layers_bin" /absolute/path/to/input scan-count 100 2 2>&1 | envctl redact
perf report --stdio -i target/benchmarks/layers.perf 2>&1 | envctl redact
strace -c "$layers_bin" /absolute/path/to/input scan-count 1 0 2>&1 | envctl redact
```

Profiles and syscall totals include startup, the oracle walks before and after measurement, one verification traversal, warmups and measured traversals. Use enough measured iterations for the workload to dominate CPU profiles. Do not divide total syscall counts by `RUNS` and report the result as one traversal. Allocation regions exclude verification and warmups. Keep traced and instrumented results separate from normal timing. `perf`, `strace` and `envctl` are optional.

## fsql layers

| Mode             | Additional work                                             |
| ---------------- | ----------------------------------------------------------- |
| walker-count     | fsql Walker entries, counted without collecting paths       |
| query-count      | prepare and stream the path query, no serialization         |
| query-lines      | path query with buffered Lines output                       |
| query-json       | path query with buffered existing JSON output               |
| query-flush      | existing JSON writer flushed per row, matching CLI behavior |
| query-direct     | experimental JSON writer for a single text column           |
| query-size-count | regular-file size predicate, no serialization               |
| query-size-json  | regular-file size predicate and buffered JSON output        |

Output goes to `/dev/null`. Queries select `path` with `depth > 0`; size queries also require `kind = 'file' AND size > 1000000`. Each measured operation includes query preparation. These are in-process measurements, so use `run bench` to include CLI startup and compare external tools. Use `cargo bench:traversal` for vector-collecting adapters and the other Rust walker libraries, and `cargo bench:engine` for Criterion engine microbenchmarks.

The [original investigation](investigations/2026-10-19-layers.md) records a baseline measured with a different methodology. Take new measurements with these probes.
