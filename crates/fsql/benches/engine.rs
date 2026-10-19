use std::fs;
use std::hint::black_box;
use std::ops::ControlFlow;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fsql::walk::WalkOptions;
use fsql::{Engine, Value};

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(files: usize) -> Self {
        let directory = tempfile::tempdir().expect("temporary benchmark directory");
        let root = directory.path().join("files");
        fs::create_dir(&root).unwrap();
        for index in 0..files {
            let parent = root.join(format!("group-{:02}", index % 10));
            fs::create_dir_all(&parent).unwrap();
            let extension = if index % 2 == 0 { "rs" } else { "txt" };
            let path = parent.join(format!("file-{index:04}.{extension}"));
            fs::write(&path, vec![b'x'; 64 + index % 1024]).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        Self { directory, root }
    }

    fn engine(&self) -> Engine {
        Engine::new(&self.root, WalkOptions::default())
    }
}

fn planning(c: &mut Criterion) {
    let engine = Engine::new(".", WalkOptions::default());
    let mut group = c.benchmark_group("prepare");
    for (name, sql) in [
        ("filter", include_str!("queries/filter.fsql")),
        ("aggregate", include_str!("queries/aggregate.fsql")),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| engine.prepare_query(black_box(sql)).unwrap())
        });
    }
    group.finish();
}

fn queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("query");
    for count in [100, 1_000] {
        let fixture = Fixture::new(count);
        let engine = fixture.engine();
        group.throughput(Throughput::Elements(count as u64));
        for (name, sql, expected_rows) in [
            ("names", include_str!("queries/names.fsql"), count),
            ("metadata", include_str!("queries/metadata.fsql"), count),
            ("top_k", include_str!("queries/top-k.fsql"), 10),
            ("aggregate", include_str!("queries/aggregate.fsql"), 2),
        ] {
            let query = engine.prepare_query(sql).unwrap();
            let (result, completion) = query.collect().unwrap();
            assert!(completion.is_complete());
            assert_eq!(result.rows.len(), expected_rows);
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter(|| black_box(query.collect().unwrap()));
            });
        }

        let query = engine
            .prepare_query(include_str!("queries/names.fsql"))
            .unwrap();
        let mut seen = 0;
        let completion = query
            .stream(&mut |_, _| {
                seen += 1;
                Ok(ControlFlow::Continue(()))
            })
            .unwrap();
        assert!(completion.is_complete());
        assert_eq!(seen, count);
        group.bench_function(BenchmarkId::new("stream_names", count), |b| {
            b.iter(|| {
                query
                    .stream(&mut |_, row: &[Value]| {
                        black_box(row);
                        Ok(ControlFlow::Continue(()))
                    })
                    .unwrap()
            });
        });
    }
    group.finish();
}

fn mutations(c: &mut Criterion) {
    let sql = include_str!("queries/chmod.fsql");
    let mut group = c.benchmark_group("mutation");
    for count in [10, 100] {
        let fixture = Fixture::new(count);
        let engine = fixture.engine();
        assert_eq!(engine.resolve_mutation(sql).unwrap().len(), count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(BenchmarkId::new("resolve_chmod", count), |b| {
            b.iter(|| black_box(engine.resolve_mutation(black_box(sql)).unwrap()));
        });
        group.bench_function(BenchmarkId::new("apply_chmod_journaled", count), |b| {
            b.iter_batched_ref(
                || {
                    let fixture = Fixture::new(count);
                    let resolved = fixture.engine().resolve_mutation(sql).unwrap();
                    let journals = fixture.directory.path().join("journals");
                    (fixture, Some(resolved), journals)
                },
                |(_, resolved, journals)| {
                    let (outcome, id) = resolved.take().unwrap().apply(journals).unwrap();
                    assert_eq!(outcome.applied, count, "{outcome:?}");
                    assert!(outcome.failures.is_empty(), "{outcome:?}");
                    assert!(outcome.partial.is_empty(), "{outcome:?}");
                    assert!(outcome.recovery_required.is_empty(), "{outcome:?}");
                    black_box((outcome, id))
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

// The same query files are used by the CLI harness. Keep the original workload
// alongside incremental improvements, so optimizations remain measurable.
const HELL: &str = include_str!("../../../benchmarks/queries/hell.fsql");
const HELL_PRUNED: &str = include_str!("../../../benchmarks/queries/hell-pruned.fsql");
const HELL_JOINED: &str = include_str!("../../../benchmarks/queries/hell-joined.fsql");
const HELL_INDEXED: &str = include_str!("../../../benchmarks/queries/hell-indexed.fsql");

fn policy_fixture(files: usize) -> Fixture {
    let fixture = Fixture::new(files);
    for index in 0..files * 4 {
        let parent = fixture.root.join(format!("target/build-{}", index % 10));
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join(format!("artifact-{index}.rs")), b"artifact").unwrap();
    }
    for name in [
        ".env.local",
        ".env.example",
        "app.min.js",
        "note.bak",
        "keep.rs",
    ] {
        fs::write(fixture.root.join(name), b"policy").unwrap();
    }
    fixture
}

fn template(sql: &str, fixture: &Fixture) -> String {
    let root = fixture.root.to_str().unwrap().replace('\'', "''");
    sql.replace("{{root}}", &format!("'{root}'"))
}

fn policy_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("policy");
    group.sample_size(10);
    for count in [100, 1_000] {
        let fixture = policy_fixture(count);
        let engine = fixture.engine();
        // Count input files, including excluded artifacts and five policy cases.
        group.throughput(Throughput::Elements((count * 5 + 5) as u64));
        let mut expected = None;
        for (name, source) in [
            ("hell", HELL),
            ("hell_pruned", HELL_PRUNED),
            ("hell_joined", HELL_JOINED),
            ("hell_indexed", HELL_INDEXED),
        ] {
            let sql = template(source, &fixture);
            let query = engine.prepare_query(&sql).unwrap();
            let (result, completion) = query.collect().unwrap();
            assert!(completion.is_complete());
            // All ordinary fixture files plus .env.example and keep.rs survive.
            assert_eq!(result.rows.len(), count + 2);
            if let Some(expected) = &expected {
                assert_eq!(&result.rows, expected, "{name}: values or order changed");
            } else {
                expected = Some(result.rows);
            }
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter(|| black_box(query.collect().unwrap()));
            });
        }
    }
    group.finish();
}

fn pruning(c: &mut Criterion) {
    let mut group = c.benchmark_group("pruning");
    for count in [100, 1_000] {
        let fixture = policy_fixture(count);
        let engine = fixture.engine();
        group.throughput(Throughput::Elements((count * 5 + 5) as u64));
        let filter = "NOT (relative_path GLOB ANY (ARRAY['**/target', '**/target/**']))";
        let mut expected = None;
        for (name, predicate) in [
            ("row_filter", format!("({filter}) IS TRUE")),
            ("subtree_pruning", filter.to_owned()),
        ] {
            // IS TRUE keeps WHERE semantics but prevents subtree extraction.
            let sql =
                format!("SELECT path FROM files WHERE kind = 'file' AND {predicate} ORDER BY path");
            let query = engine.prepare_query(&sql).unwrap();
            let (result, completion) = query.collect().unwrap();
            assert!(completion.is_complete());
            assert_eq!(result.rows.len(), count + 5);
            if let Some(expected) = &expected {
                assert_eq!(&result.rows, expected);
            } else {
                expected = Some(result.rows);
            }
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter(|| black_box(query.collect().unwrap()));
            });
        }
    }
    group.finish();
}

fn relational(c: &mut Criterion) {
    let engine = Engine::new(".", WalkOptions::default());
    let mut group = c.benchmark_group("relational");
    group.sample_size(10);
    for count in [100, 1_000] {
        // Pure relational workload: no filesystem or serialization in timings.
        let rows = (0..count)
            .map(|n| format!("({n})"))
            .collect::<Vec<_>>()
            .join(",");
        for (name, join) in [
            ("on_equality", "ON a.id = b.id"),
            ("using_equality", "USING (id)"),
        ] {
            let sql = format!(
                "WITH a(id) AS (VALUES {rows}), b(id) AS (VALUES {rows}) SELECT a.id FROM a JOIN b {join} ORDER BY a.id"
            );
            let query = engine.prepare_query(&sql).unwrap();
            let (result, completion) = query.collect().unwrap();
            assert!(completion.is_complete());
            assert_eq!(
                result.rows,
                (0..count).map(|n| vec![Value::Int(n)]).collect::<Vec<_>>()
            );
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter(|| black_box(query.collect().unwrap()));
            });
        }
    }
    group.finish();
}

fn glob_sets(c: &mut Criterion) {
    let engine = Engine::new(".", WalkOptions::default());
    let mut group = c.benchmark_group("glob_sets");
    for count in [100, 1_000] {
        let rows = (0..count)
            .map(|n| {
                let ext = if n % 2 == 0 { "rs" } else { "o" };
                format!("('src/file-{n}.{ext}')")
            })
            .collect::<Vec<_>>()
            .join(",");
        for (name, predicate) in [
            (
                "scalar_or",
                "path GLOB '**/*.rs' OR path GLOB '**/*.md' OR path GLOB '**/*.toml'",
            ),
            (
                "constant_any",
                "path GLOB ANY (ARRAY['**/*.rs', '**/*.md', '**/*.toml'])",
            ),
        ] {
            let sql = format!(
                "WITH input(path) AS (VALUES {rows}) SELECT path FROM input WHERE {predicate}"
            );
            let query = engine.prepare_query(&sql).unwrap();
            let (result, completion) = query.collect().unwrap();
            assert!(completion.is_complete());
            assert_eq!(result.rows.len(), count / 2);
            group.bench_function(BenchmarkId::new(name, count), |b| {
                b.iter(|| black_box(query.collect().unwrap()));
            });
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    planning,
    queries,
    mutations,
    policy_queries,
    pruning,
    relational,
    glob_sets
);
criterion_main!(benches);
