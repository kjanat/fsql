use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::UNIX_EPOCH;

use fsql::output::ResultSet;
use fsql::walk::WalkOptions;
use fsql::{Engine, Value};

const LARGEST: &str = include_str!("../examples/queries/largest-files.fsql");
const DIRECTORIES: &str = include_str!("../../../examples/directory-usage.fsql");
const DUPLICATES: &str = include_str!("../../../examples/duplicate-sizes.fsql");
const EXECUTABLES: &str = include_str!("../../../examples/largest-executables.fsql");
const WRITABLE: &str = include_str!("../../../examples/world-writable-recent.fsql");
const EXTENSIONS: &str = include_str!("../../../examples/by-extension.fsql");

fn query(root: &Path, sql: &str) -> ResultSet {
    let (set, completion) = Engine::new(root, WalkOptions::default())
        .prepare_query(sql)
        .unwrap()
        .collect()
        .unwrap();
    assert!(completion.is_complete(), "{:?}", completion.diagnostics);
    set
}

#[test]
fn largest_files_reports_whole_tree_totals_before_limit() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    fs::create_dir_all(root.join("nested/deep")).unwrap();
    for index in 1..=12 {
        fs::write(
            root.join(format!("nested/file-{index}.txt")),
            vec![b'x'; index * 1024],
        )
        .unwrap();
    }
    symlink("nested/file-1.txt", root.join("file-link")).unwrap();
    symlink("nested", root.join("dir-link")).unwrap();
    let set = query(root, LARGEST);
    assert_eq!(
        set.headers,
        [
            "path",
            "readable_size",
            "size",
            "perms",
            "total_files",
            "total_dirs"
        ]
    );
    assert_eq!(set.rows.len(), 10);
    for (index, row) in set.rows.iter().enumerate() {
        assert_eq!(row[2], Value::Int(((12 - index) * 1024) as i64));
        assert_eq!(row[4], Value::Int(12));
        assert_eq!(row[5], Value::Int(3));
    }
}

#[test]
fn empty_tree_still_reports_directory_totals() {
    let fixture = tempfile::tempdir().unwrap();
    fs::create_dir(fixture.path().join("empty")).unwrap();
    let set = query(fixture.path(), LARGEST);
    assert_eq!(
        set.rows,
        vec![vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int(0),
            Value::Int(2),
        ]]
    );
}

#[test]
fn recursive_directory_totals_stay_within_the_root() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    fs::create_dir_all(root.join("nested/deep")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::write(root.join("a"), b"a").unwrap();
    fs::write(root.join("nested/b"), b"bb").unwrap();
    fs::write(root.join("nested/deep/c"), b"ccc").unwrap();
    symlink("nested", root.join("link")).unwrap();
    let set = query(root, DIRECTORIES);
    assert_eq!(set.rows.len(), 3);
    for (row, (path, count, bytes)) in set.rows.iter().zip([
        (root.to_path_buf(), 3, 6),
        (root.join("nested"), 2, 5),
        (root.join("nested/deep"), 1, 3),
    ]) {
        assert_eq!(row[0], Value::Text(path.to_str().unwrap().into()));
        assert_eq!(row[1], Value::Int(count));
        assert_eq!(row[2], Value::Int(bytes));
    }
}

#[test]
fn duplicate_sizes_report_candidates_and_skip_empty_files() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    for (name, content) in [
        ("a", "abc"),
        ("b", "xyz"),
        ("c", "four"),
        ("empty-a", ""),
        ("empty-b", ""),
    ] {
        fs::write(root.join(name), content).unwrap();
    }
    let set = query(root, DUPLICATES);
    assert_eq!(set.rows.len(), 2);
    for row in &set.rows {
        assert_eq!(row[1], Value::Int(3));
        assert_eq!(row[3], Value::Int(2));
    }
}

#[test]
fn permission_reports_filter_execute_write_and_modification_time() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    for (name, mode) in [
        ("execute", 0o755),
        ("writable", 0o666),
        ("old", 0o666),
        ("ordinary", 0o644),
    ] {
        let path = root.join(name);
        fs::write(&path, "data").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    fs::File::open(root.join("old"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
        .unwrap();
    for (sql, name) in [(EXECUTABLES, "execute"), (WRITABLE, "writable")] {
        let set = query(root, sql);
        assert_eq!(set.rows.len(), 1);
        assert_eq!(
            set.rows[0][0],
            Value::Text(root.join(name).to_str().unwrap().into())
        );
    }
}

#[test]
fn extension_summary_keeps_exact_byte_totals() {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(fixture.path().join("a.txt"), b"ab").unwrap();
    fs::write(fixture.path().join("b.txt"), b"cdef").unwrap();
    let set = query(fixture.path(), EXTENSIONS);
    assert_eq!(
        set.rows,
        vec![vec![
            Value::Text("txt".into()),
            Value::Int(2),
            Value::Int(6),
            Value::Text("6 B".into()),
            Value::Text("3 B".into()),
            Value::Text("4 B".into()),
        ]]
    );
}
