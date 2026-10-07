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

criterion_group!(benches, planning, queries, mutations);
criterion_main!(benches);
