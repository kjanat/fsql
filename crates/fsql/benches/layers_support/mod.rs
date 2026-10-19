mod driver;
mod raw;
use std::hint::black_box;
use std::io::Write;
use std::path::Path;

pub const MODES: &[&str] = &[
    "std-count",
    "scan-count",
    "scan-anchors",
    "scan-paths",
    "scan-stats",
    "walker-count",
    "query-count",
    "query-lines",
    "query-json",
    "query-flush",
    "query-direct",
    "query-size-count",
    "query-size-json",
];

pub fn measure(root: &Path, mode: &str, visit: &mut dyn FnMut(&Path), verify: bool) -> usize {
    if mode == "std-count" {
        if verify {
            for path in driver::inventory(root, false) {
                visit(&path);
            }
        }
        return driver::std_count(root);
    }
    if mode.starts_with("scan-") {
        return raw::scan(root, mode, visit, verify);
    }
    if mode == "walker-count" {
        let mut count = 0;
        for entry in fsql::walk::Walker::new(root, Default::default()).unwrap() {
            let entry = entry.unwrap();
            if !entry.relative_path().as_os_str().is_empty() {
                count += 1;
                if verify {
                    visit(entry.path());
                }
            }
            black_box(entry);
        }
        return count;
    }
    // Preparation is intentionally included: this measures one complete engine call.
    let engine = fsql::Engine::new(root, Default::default());
    let query = engine
        .prepare_query(if mode.contains("size") {
            "select path from files where depth > 0 and kind = 'file' and size > 1000000"
        } else {
            "select path from files where depth > 0"
        })
        .unwrap();
    let mut count = 0;
    let mut out = std::io::BufWriter::new(std::fs::File::create("/dev/null").unwrap());
    let completion = query
        .stream(&mut |headers, row| {
            count += 1;
            let fsql::Value::Text(path) = &row[0] else {
                panic!("expected path text")
            };
            if verify {
                visit(Path::new(path));
            }
            black_box(row);
            if mode == "query-direct" {
                // Experimental single-column baseline, not a replacement general serializer.
                out.write_all(b"{\"path\":").unwrap();
                serde_json::to_writer(&mut out, path).unwrap();
                out.write_all(b"}\n").unwrap();
            } else if !mode.ends_with("count") {
                let format = if mode == "query-lines" {
                    fsql::output::Format::Lines
                } else {
                    fsql::output::Format::Json
                };
                fsql::output::write_row(&mut out, headers, row, format).unwrap();
                if mode == "query-flush" {
                    out.flush().unwrap();
                }
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    out.flush().unwrap();
    assert!(completion.is_complete());
    count
}

pub fn main(allocations: bool) {
    driver::main(allocations);
}
