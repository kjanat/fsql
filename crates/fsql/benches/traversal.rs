//! End-to-end in-process traversal, including path collection and worker startup.
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tree_fucker::domain::DomainCrossing;
use tree_fucker::path::RelativePath;
use tree_fucker::policy::{PathPredicate, ScanDecision};
use tree_fucker::scan::{Scan, ScanEvent, ScanOptions};
use tree_fucker::std_fs::StdFileSystem;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Work {
    Paths,
    Metadata,
    Exclude,
    FirstTen,
}

fn excluded(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("target" | "node_modules" | ".git")
        )
    })
}

fn accept(root: &Path, path: &Path, work: Work) -> bool {
    let relative = path.strip_prefix(root).unwrap();
    if relative.as_os_str().is_empty() || (work == Work::Exclude && excluded(relative)) {
        return false;
    }
    if work == Work::Metadata {
        let metadata = path.symlink_metadata().unwrap();
        return metadata.is_file() && metadata.len() > 100;
    }
    true
}

fn push(rows: &mut Vec<PathBuf>, root: &Path, path: &Path, work: Work) -> bool {
    if accept(root, path, work) {
        rows.push(path.to_path_buf());
    }
    work == Work::FirstTen && rows.len() == 10
}

fn std_walk(root: &Path, work: Work) -> Vec<PathBuf> {
    let mut rows = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if work == Work::Exclude && excluded(path.strip_prefix(root).unwrap()) {
                continue;
            }
            if push(&mut rows, root, &path, work) {
                return rows;
            }
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            }
        }
    }
    rows
}

// Each ignore worker buffers locally; merging happens only when it finishes.
struct Batch<'a> {
    rows: Vec<PathBuf>,
    output: &'a Mutex<Vec<PathBuf>>,
}
impl Drop for Batch<'_> {
    fn drop(&mut self) {
        self.output.lock().unwrap().append(&mut self.rows);
    }
}

fn walk(name: &str, root: &Path, work: Work) -> Vec<PathBuf> {
    let mut rows = Vec::new();
    match name {
        "std" => return std_walk(root, work),
        "walkdir" => {
            let iter = walkdir::WalkDir::new(root)
                .min_depth(1)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    work != Work::Exclude || !excluded(e.path().strip_prefix(root).unwrap())
                });
            for entry in iter {
                if push(&mut rows, root, entry.unwrap().path(), work) {
                    break;
                }
            }
        }
        "ignore-serial" | "ignore-parallel-4" => {
            let mut builder = ignore::WalkBuilder::new(root);
            builder
                .standard_filters(false)
                .follow_links(false)
                .threads(4);
            let base = root.to_path_buf();
            builder.filter_entry(move |e| {
                work != Work::Exclude || !excluded(e.path().strip_prefix(&base).unwrap())
            });
            if name == "ignore-serial" {
                for entry in builder.build() {
                    if push(&mut rows, root, entry.unwrap().path(), work) {
                        break;
                    }
                }
            } else {
                let output = Mutex::new(Vec::new());
                let count = AtomicUsize::new(0);
                builder.build_parallel().run(|| {
                    let mut batch = Batch {
                        rows: Vec::new(),
                        output: &output,
                    };
                    let count = &count;
                    Box::new(move |entry| {
                        let entry = entry.unwrap();
                        if accept(root, entry.path(), work) {
                            if work == Work::FirstTen && count.fetch_add(1, Ordering::Relaxed) >= 10
                            {
                                return ignore::WalkState::Quit;
                            }
                            batch.rows.push(entry.into_path());
                            if work == Work::FirstTen && count.load(Ordering::Relaxed) >= 10 {
                                return ignore::WalkState::Quit;
                            }
                        }
                        ignore::WalkState::Continue
                    })
                });
                rows = output.into_inner().unwrap();
            }
        }
        "jwalk-serial" | "jwalk-parallel-4" => {
            // Join every pool worker before returning, including after early stop.
            let handles = Arc::new(Mutex::new(Vec::new()));
            let pool = (name == "jwalk-parallel-4").then(|| {
                let handles = Arc::clone(&handles);
                Arc::new(
                    jwalk::rayon::ThreadPoolBuilder::new()
                        .num_threads(4)
                        .spawn_handler(move |thread| {
                            let handle = std::thread::Builder::new().spawn(move || thread.run())?;
                            handles.lock().unwrap().push(handle);
                            Ok(())
                        })
                        .build()
                        .unwrap(),
                )
            });
            let parallelism = pool.as_ref().map_or(jwalk::Parallelism::Serial, |pool| {
                jwalk::Parallelism::RayonExistingPool {
                    pool: Arc::clone(pool),
                    busy_timeout: Some(Duration::from_secs(10)),
                }
            });
            let iter = jwalk::WalkDir::new(root)
                .min_depth(0)
                .skip_hidden(false)
                .follow_links(false)
                .parallelism(parallelism)
                .process_read_dir(move |_, _, _, children| {
                    if work == Work::Exclude {
                        children.retain(|entry| match entry {
                            Ok(entry) => !matches!(
                                entry.file_name.to_str(),
                                Some("target" | "node_modules" | ".git")
                            ),
                            Err(_) => true,
                        });
                    }
                });
            for entry in iter {
                let entry = entry.unwrap();
                if let Some(children) = &entry.read_children {
                    assert!(
                        children.error().is_none(),
                        "jwalk read failure: {:?}",
                        children.error()
                    );
                }
                if push(&mut rows, root, &entry.path(), work) {
                    break;
                }
            }
            drop(pool);
            for handle in handles.lock().unwrap().drain(..) {
                handle.join().unwrap();
            }
        }
        "tree-fucker" => {
            let policy = Arc::new(PathPredicate::new(move |path: &RelativePath, _| {
                if work == Work::Exclude
                    && matches!(
                        path.file_name().and_then(|name| name.to_str()),
                        Some("target" | "node_modules" | ".git")
                    )
                {
                    ScanDecision::Excluded
                } else {
                    ScanDecision::Eligible {
                        initially_loaded: true,
                    }
                }
            }));
            let scan = Scan::open(
                Arc::new(StdFileSystem::new()),
                root.to_path_buf(),
                policy,
                ScanOptions {
                    crossing: DomainCrossing::Follow,
                    ceiling: Duration::MAX,
                    ..ScanOptions::default()
                },
            )
            .unwrap();
            for event in scan {
                match event.unwrap() {
                    ScanEvent::Entry(entry) => {
                        if push(&mut rows, root, &entry.path.under(root), work) {
                            break;
                        }
                    }
                    ScanEvent::Unlisted { failure, .. } => {
                        panic!("unlisted directory: {failure:?}")
                    }
                    ScanEvent::Boundary { .. } => {}
                }
            }
        }
        "fsql-walker" => {
            for entry in fsql::walk::Walker::new(root, fsql::walk::WalkOptions::default()).unwrap()
            {
                if push(&mut rows, root, entry.unwrap().path(), work) {
                    break;
                }
            }
        }
        _ => unreachable!(),
    }
    rows
}

fn fixture(shape: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    for i in 0..300 {
        let dir = match shape {
            "wide" => temp.path().join(format!("dir-{i}")),
            "deep" => temp.path().join("d/".repeat(i % 32 + 1)),
            "excluded" => temp
                .path()
                .join(if i % 5 == 0 { "src" } else { "target" })
                .join(format!("dir-{i}")),
            _ => unreachable!(),
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("file-{i}.rs")),
            vec![b'x'; if i % 2 == 0 { 101 } else { 100 }],
        )
        .unwrap();
    }
    std::fs::write(temp.path().join(".hidden\nfile"), "hidden").unwrap();
    std::fs::create_dir_all(temp.path().join("src/node_modules/nested")).unwrap();
    std::fs::create_dir_all(temp.path().join(".git/objects")).unwrap();
    if shape != "excluded" {
        std::fs::write(temp.path().join("target"), "excluded file").unwrap();
    }
    std::os::unix::fs::symlink("missing", temp.path().join("broken-link")).unwrap();
    std::os::unix::fs::symlink("src", temp.path().join("directory-link")).unwrap();
    temp
}

fn traversal(c: &mut Criterion) {
    let _governor = fsql::walk::install_governor().unwrap();
    for shape in ["wide", "deep", "excluded"] {
        let fixture = fixture(shape);
        let root = fixture.path();
        let all = std_walk(root, Work::Paths);
        let mut group = c.benchmark_group(format!("traversal/{shape}"));
        group.sample_size(10);
        for work in [Work::Paths, Work::Metadata, Work::Exclude, Work::FirstTen] {
            let mut expected: Vec<_> = all
                .iter()
                .filter(|p| accept(root, p, work))
                .cloned()
                .collect();
            expected.sort();
            for name in [
                "std",
                "walkdir",
                "ignore-serial",
                "ignore-parallel-4",
                "jwalk-serial",
                "jwalk-parallel-4",
                "tree-fucker",
                "fsql-walker",
            ] {
                let mut actual = walk(name, root, work);
                actual.sort();
                if work == Work::FirstTen {
                    assert_eq!(actual.len(), 10, "{name}/{shape}");
                    assert!(actual.windows(2).all(|p| p[0] != p[1]));
                    assert!(actual.iter().all(|p| expected.binary_search(p).is_ok()));
                } else {
                    assert_eq!(actual, expected, "{name}/{shape}/{work:?}");
                }
                let label = if name == "fsql-walker" && work == Work::Exclude {
                    "fsql-walker-row-filter"
                } else {
                    name
                };
                group.bench_with_input(
                    BenchmarkId::new(format!("{work:?}"), label),
                    &work,
                    |b, &work| b.iter(|| black_box(walk(name, root, work))),
                );
            }
        }
        group.finish();
    }
}
criterion_group!(benches, traversal);
criterion_main!(benches);
