use std::fs;
use std::ops::ControlFlow;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use fsql::journal::{self, Journal, Record};
use fsql::walk::WalkOptions;
use fsql::{Engine, Error, ErrorPolicy, Value};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsql-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn engine(&self) -> Engine {
        Engine::new(&self.0, WalkOptions::default())
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn numeric_equivalence_is_shared_by_sql_operators() {
    let fx = Fixture::new();
    let engine = fx.engine();
    for sql in [
        "select distinct column1 from (values (1), (1.0)) v",
        "select 1 intersect select 1.0",
        "select column1 from (values (1), (1.0)) v group by column1",
    ] {
        let (set, _) = engine.prepare_query(sql).unwrap().collect().unwrap();
        assert_eq!(set.rows.len(), 1, "{sql}");
    }
    let (set, _) = engine
        .prepare_query("select 9007199254740993 = 9007199254740992.0, 0 = -0.0")
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(set.rows[0], vec![Value::Bool(false), Value::Bool(true)]);
    let (set, _) = engine
        .prepare_query("select count(distinct column1) from (values (1), (1.0), (null)) v")
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(set.rows[0][0], Value::Int(1));
}

#[test]
fn invalid_sql_is_rejected_before_any_scan() {
    let engine = Engine::new("/definitely/missing/fsql", WalkOptions::default());
    for sql in [
        "select nonsense from files where false",
        "select bogus(name) from files where false",
        "select lower(name, name) from files where false",
        "select name, sum(size) over () from files",
        "select name, count(*) from files",
        "select sum(count(*)) from files",
        "select a.name from files a join files b on a.missing = b.name",
    ] {
        assert!(engine.prepare_query(sql).is_err(), "{sql}");
    }
    assert!(
        engine
            .prepare_query("select lower(ext), count(*) from files group by ext")
            .is_ok()
    );
    assert!(
        engine
            .prepare_query("select ext, count(*) from files group by lower(ext)")
            .is_err()
    );
}

#[test]
fn mutation_paths_do_not_silently_discard_parent_components() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.path("a/b")).unwrap();
    let sql = format!(
        "insert into files (path, content) values ('{}/a/b/../wrong', 'x')",
        fx.0.display()
    );
    assert!(fx.engine().resolve_mutation(&sql).is_err());
    assert!(!fx.path("a/b/wrong").exists());
}

#[test]
fn one_filesystem_cannot_be_bypassed_by_an_exact_path_predicate() {
    if fs::metadata("/dev").unwrap().dev() == fs::metadata("/dev/shm").unwrap().dev() {
        return;
    }
    let file = PathBuf::from(format!("/dev/shm/fsql-contract-{}", std::process::id()));
    fs::write(&file, b"x").unwrap();
    let engine = Engine::new(
        "/dev",
        WalkOptions {
            one_filesystem: true,
            max_depth: Some(3),
            initial_depth: 0,
        },
    );
    let result = engine
        .prepare_query(&format!(
            "select path from files where path = '{}'",
            file.display()
        ))
        .unwrap()
        .collect();
    fs::remove_file(file).unwrap();
    assert!(result.unwrap().0.rows.is_empty());
}

#[test]
fn strict_and_best_effort_scans_have_distinct_contracts() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let fx = Fixture::new();
    let dir = fx.path("blocked");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("hidden"), b"x").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o0)).unwrap();
    let mut engine = fx.engine();
    let strict = engine
        .prepare_query("select count(*) from files")
        .unwrap()
        .collect();
    engine.execution.error_policy = ErrorPolicy::BestEffort;
    let partial = engine
        .prepare_query("select count(*) from files")
        .unwrap()
        .collect();
    let mutation = engine.resolve_mutation("delete from files where name = 'hidden'");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(strict.is_err());
    assert!(mutation.is_err());
    let (set, completion) = partial.unwrap();
    assert!(!completion.is_complete());
    assert_eq!(set.rows[0][0], Value::Int(2));
}

#[test]
fn update_undo_refuses_a_replacement_object() {
    let fx = Fixture::new();
    let path = fx.path("victim");
    fs::write(&path, b"old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let base = fx.path("journal");
    let (outcome, id) = fx
        .engine()
        .resolve_mutation("update files set mode = 0o644 where name = 'victim'")
        .unwrap()
        .apply(&base)
        .unwrap();
    assert_eq!(outcome.applied, 1);
    fs::rename(&path, fx.path("original")).unwrap();
    fs::write(&path, b"replacement").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let outcome = fsql::mutate::undo(&base, &id).unwrap();
    assert_eq!(outcome.applied, 0);
    assert!(matches!(&outcome.failures[0].1, Error::Stale(_)));
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn insert_source_failure_leaves_no_unrecorded_destination() {
    let fx = Fixture::new();
    let source = fx.path("source");
    fs::write(&source, b"x").unwrap();
    let sql = format!(
        "insert into files (path, source) values ('{}', '{}')",
        fx.path("copy").display(),
        source.display()
    );
    let mutation = fx.engine().resolve_mutation(&sql).unwrap();
    fs::remove_file(source).unwrap();
    let (outcome, id) = mutation.apply(&fx.path("journal")).unwrap();
    assert_eq!(outcome.applied, 0);
    assert!(!fx.path("copy").exists());
    assert!(journal::load(&fx.path("journal"), &id).unwrap().is_empty());
}

#[test]
fn interrupted_intents_are_visible_and_block_automatic_undo() {
    let fx = Fixture::new();
    let mut log = Journal::open(&fx.path("journal"), "insert").unwrap();
    log.begin(&Record::Insert {
        seq: 0,
        path: fx.path("file").as_os_str().as_encoded_bytes().to_vec(),
        kind: "file".into(),
        dev: 0,
        ino: 0,
        ctime: 0,
    })
    .unwrap();
    assert!(matches!(
        journal::load(&fx.path("journal"), log.id()),
        Err(Error::RecoveryRequired(_))
    ));
    assert!(journal::list(&fx.path("journal")).unwrap()[0].recovery_required);
    assert!(journal::load(&fx.path("journal"), "../escape").is_err());
}

#[test]
fn limits_and_cancellation_bound_execution_and_streaming() {
    let fx = Fixture::new();
    for n in 0..5 {
        fs::write(fx.path(&n.to_string()), b"x").unwrap();
    }
    let mut engine = fx.engine();
    engine.execution.max_rows = 1;
    assert!(matches!(
        engine
            .prepare_query("select path from files")
            .unwrap()
            .collect(),
        Err(Error::ResourceLimit(_))
    ));
    let mut count = 0;
    let completion = engine
        .prepare_query("select path from files")
        .unwrap()
        .stream(&mut |_, _| {
            count += 1;
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(count, 6);
    assert_eq!(completion.rows, 6);
    let completion = engine
        .prepare_query("select path from files")
        .unwrap()
        .stream(&mut |_, _| Ok(ControlFlow::Break(())))
        .unwrap();
    assert_eq!(completion.rows, 1);
    engine.execution.cancellation.cancel();
    assert!(matches!(
        engine
            .prepare_query("select path from files")
            .unwrap()
            .collect(),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn top_k_is_bounded_and_using_joins_share_numeric_keys() {
    let fx = Fixture::new();
    for n in 0..10 {
        fs::write(fx.path(&n.to_string()), vec![0; n]).unwrap();
    }
    let mut engine = fx.engine();
    engine.execution.max_rows = 2;
    let (set, _) = engine
        .prepare_query("select size from files where kind = 'file' order by size desc limit 2")
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(set.rows, vec![vec![Value::Int(9)], vec![Value::Int(8)]]);
    engine.execution.max_rows = 100;
    let (set, _) = engine
        .prepare_query(
            "select a.column1 from (values (1), (2)) a join (values (1.0)) b using (column1)",
        )
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(set.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn library_mutation_cap_is_enforced_before_apply() {
    let fx = Fixture::new();
    fs::write(fx.path("a"), b"x").unwrap();
    fs::write(fx.path("b"), b"x").unwrap();
    let mut engine = fx.engine();
    engine.mutation_cap = 1;
    assert!(
        engine
            .resolve_mutation("delete from files where kind = 'file'")
            .is_err()
    );
    assert!(fx.path("a").exists());
    assert!(fx.path("b").exists());
}

#[test]
fn a_partial_insert_is_recorded_and_can_be_undone() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let fx = Fixture::new();
    let sql = format!(
        "insert into files (path, content, uid) values ('{}', 'written', 0)",
        fx.path("partial").display()
    );
    let base = fx.path("journal");
    let (outcome, id) = fx
        .engine()
        .resolve_mutation(&sql)
        .unwrap()
        .apply(&base)
        .unwrap();
    assert_eq!(outcome.applied, 0);
    assert_eq!(outcome.partial.len(), 1);
    assert_eq!(fs::read(fx.path("partial")).unwrap(), b"written");
    assert_eq!(journal::load(&base, &id).unwrap().len(), 1);
    let undone = fsql::mutate::undo(&base, &id).unwrap();
    assert_eq!(undone.applied, 1);
    assert!(!fx.path("partial").exists());
}

#[test]
fn a_changed_insert_source_is_rejected_before_creation() {
    let fx = Fixture::new();
    fs::write(fx.path("source"), b"old").unwrap();
    let sql = format!(
        "insert into files (path, source) values ('{}', '{}')",
        fx.path("copy").display(),
        fx.path("source").display()
    );
    let mutation = fx.engine().resolve_mutation(&sql).unwrap();
    fs::rename(fx.path("source"), fx.path("old")).unwrap();
    fs::write(fx.path("source"), b"replacement").unwrap();
    let (outcome, _) = mutation.apply(&fx.path("journal")).unwrap();
    assert_eq!(outcome.applied, 0);
    assert!(matches!(&outcome.failures[0].1, Error::Stale(_)));
    assert!(!fx.path("copy").exists());
}

#[test]
fn chmod_can_restore_an_unreadable_owned_file_without_following_links() {
    let fx = Fixture::new();
    let path = fx.path("unreadable");
    fs::write(&path, b"x").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    let (outcome, _) = fx
        .engine()
        .resolve_mutation("update files set mode = 0o600 where name = 'unreadable'")
        .unwrap()
        .apply(&fx.path("journal"))
        .unwrap();
    assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn zero_limit_does_not_scan_and_cancellation_still_wins() {
    let engine = Engine::new("/definitely/missing/fsql", WalkOptions::default());
    let query = engine
        .prepare_query("select path from files limit 0")
        .unwrap();
    assert!(query.collect().unwrap().0.rows.is_empty());
    assert_eq!(
        query
            .stream(&mut |_, _| panic!("unexpected row"))
            .unwrap()
            .rows,
        0
    );
    engine.execution.cancellation.cancel();
    assert!(matches!(query.collect(), Err(Error::ResourceLimit(_))));
}

#[test]
fn counting_does_not_retain_every_input_row() {
    let fx = Fixture::new();
    for n in 0..10 {
        fs::write(fx.path(&n.to_string()), b"x").unwrap();
    }
    let mut engine = fx.engine();
    engine.execution.max_rows = 2;
    let (set, _) = engine
        .prepare_query("select count(*) from files")
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(set.rows, vec![vec![Value::Int(11)]]);
}
