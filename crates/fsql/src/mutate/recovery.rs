//! Durable per-entry redo/undo.
//!
//! Prepared objects stay on the target filesystem.
//! Every externally visible step has an idempotent, identity-checked inverse.
use super::*;
use serde::{Deserialize, Serialize};
use std::fs;

fn bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}
fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}
fn save<T: Serialize>(file: &Path, value: &T) -> Result<()> {
    journal::atomic_write(
        file,
        &serde_json::to_vec(value).map_err(|e| Error::Plan(e.to_string()))?,
    )
}
fn read<T: serde::de::DeserializeOwned>(file: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(file).map_err(|e| std_io(file, e))?)
        .map_err(|e| Error::Plan(format!("{}: {e}", file.display())))
}
fn remove_file(file: &Path) -> Result<()> {
    match fs::remove_file(file) {
        Ok(()) => journal::sync_dir(file.parent().expect("parent")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            journal::sync_dir(file.parent().expect("parent"))
        }
        Err(e) => Err(std_io(file, e)),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Snapshot {
    identity: Identity,
    birth: Option<i64>,
    kind: String,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: i64,
    atime: i64,
    target: Option<Vec<u8>>,
}
impl Snapshot {
    fn get(p: &Path) -> Result<Option<Self>> {
        let (parent, name) = split(p)?;
        let stat = match rustix::fs::statx(&parent, &name, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(io(p, e)),
        };
        let kind = Kind::from_mode(u32::from(stat.stx_mode));
        let target = if kind == Kind::Symlink {
            Some(
                rustix::fs::readlinkat(&parent, &name, Vec::new())
                    .map_err(|e| io(p, e))?
                    .into_bytes(),
            )
        } else {
            None
        };
        Ok(Some(Self {
            identity: Identity::of(&stat),
            birth: (stat.stx_mask & rustix::fs::StatxFlags::BTIME.bits() != 0)
                .then(|| time::from_parts(stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec).0),
            kind: kind.as_str().into(),
            mode: u32::from(stat.stx_mode) & 0o7777,
            uid: stat.stx_uid,
            gid: stat.stx_gid,
            size: stat.stx_size,
            mtime: time::from_parts(stat.stx_mtime.tv_sec, stat.stx_mtime.tv_nsec).0,
            atime: time::from_parts(stat.stx_atime.tv_sec, stat.stx_atime.tv_nsec).0,
            target,
        }))
    }
    fn required(p: &Path) -> Result<Self> {
        Self::get(p)?.ok_or_else(|| Error::Stale(p.to_owned()))
    }
    // ctime changes during rename/chmod and cannot be predicted before a crash.
    // Completed operations additionally check their recorded ctime before undo.
    fn same_object(&self, other: &Self) -> bool {
        self.identity.dev == other.identity.dev
            && self.identity.ino == other.identity.ino
            && self.birth == other.birth
            && self.kind == other.kind
            && self.target == other.target
            && (self.kind == "dir" || self.size == other.size)
    }
    fn matches(&self, other: &Self) -> bool {
        self.same_object(other)
            && self.mode == other.mode
            && self.uid == other.uid
            && self.gid == other.gid
            && (self.kind == "dir" || self.mtime == other.mtime)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "step")]
enum Step {
    Move {
        from: Vec<u8>,
        to: Vec<u8>,
        object: Snapshot,
        empty: bool,
    },
    Attributes {
        path: Vec<u8>,
        before: Snapshot,
        after: Snapshot,
        atime: bool,
    },
}
impl Step {
    fn inverse(&self) -> Self {
        match self {
            Self::Move {
                from,
                to,
                object,
                empty,
            } => Self::Move {
                from: to.clone(),
                to: from.clone(),
                object: object.clone(),
                empty: *empty,
            },
            Self::Attributes {
                path,
                before,
                after,
                atime,
            } => Self::Attributes {
                path: path.clone(),
                before: after.clone(),
                after: before.clone(),
                atime: *atime,
            },
        }
    }
    fn run(&self) -> Result<()> {
        match self {
            Self::Move {
                from,
                to,
                object,
                empty,
            } => {
                let (from, to) = (path(from), path(to));
                match (Snapshot::get(from)?, Snapshot::get(to)?) {
                    (None, Some(found)) if object.matches(&found) => {}
                    (Some(found), None) if object.matches(&found) => {
                        let (parent, name) = split(from)?;
                        if *empty && object.kind == "dir" && !is_empty_dir(&parent, &name, from)? {
                            return Err(Error::Plan(format!(
                                "{} is no longer empty",
                                from.display()
                            )));
                        }
                        rename(&parent, &name, from, to)?;
                        checkpoint("move-applied");
                        let found = Snapshot::required(to)?;
                        if !object.matches(&found) {
                            return Err(Error::Stale(to.to_owned()));
                        }
                    }
                    _ => {
                        return Err(Error::Plan(format!(
                            "recovery conflict moving {} to {}; preserve both paths and move the conflicting object aside before retrying",
                            from.display(),
                            to.display()
                        )));
                    }
                }
                journal::sync_dir(from.parent().expect("parent"))?;
                // Moving an existing file must persist its data as well as its
                // name, including dirty bytes written before this statement.
                let destination = open_chain(to.parent().expect("parent"))?;
                rustix::fs::syncfs(&destination).map_err(|e| io(to, e))
            }
            Self::Attributes {
                path: p,
                before,
                after,
                atime,
            } => {
                let p = path(p);
                let current = Snapshot::required(p)?;
                let between = |n, a, b| n == a || n == b;
                if !before.same_object(&current)
                    || !(between(current.mode, before.mode, after.mode)
                        || ((before.uid != after.uid || before.gid != after.gid)
                            && (current.mode == before.mode & !0o6000
                                || current.mode == after.mode & !0o6000)))
                    || !between(current.uid, before.uid, after.uid)
                    || !between(current.gid, before.gid, after.gid)
                    || (current.kind != "dir"
                        && current.mtime != before.mtime
                        && current.mtime != after.mtime)
                {
                    return Err(Error::Stale(p.to_owned()));
                }
                let (parent, name) = split(p)?;
                let pinned = pin(&parent, &name, p)?;
                let stat = rustix::fs::statx(
                    &pinned,
                    c"",
                    AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
                    STATX_MASK,
                )
                .map_err(|e| io(p, e))?;
                if Identity::of(&stat) != current.identity {
                    return Err(Error::Stale(p.to_owned()));
                }
                let empty = c"".to_owned();
                if current.uid != after.uid || current.gid != after.gid {
                    set_owner(&pinned, &empty, p, Some(after.uid), Some(after.gid))?;
                    checkpoint("owner-applied");
                }
                if current.kind != "symlink" {
                    set_mode(&pinned, &empty, p, kind_of(&current.kind), after.mode)?;
                    checkpoint("mode-applied");
                }
                set_times(
                    &pinned,
                    &empty,
                    p,
                    atime.then_some(after.atime),
                    (before.mtime != after.mtime).then_some(after.mtime),
                )?;
                rustix::fs::syncfs(&parent).map_err(|e| io(p, e))?;
                checkpoint("times-applied");
                if !after.matches(&Snapshot::required(p)?) {
                    return Err(Error::Stale(p.to_owned()));
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Operation {
    version: u32,
    seq: u64,
    path: Vec<u8>,
    stage: Vec<u8>,
    stage_identity: Identity,
    steps: Vec<Step>,
    record: Record,
}

#[derive(Serialize, Deserialize)]
struct Preparation {
    stage: Vec<u8>,
    identity: Option<Identity>,
}

fn operation_file(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("replay-{seq}.json"))
}
fn completed_file(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("complete-{seq}.json"))
}

fn operations(dir: &Path) -> Result<Vec<Operation>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| std_io(dir, e))? {
        let entry = entry.map_err(|e| std_io(dir, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("replay-") && name.ends_with(".json") {
            let op: Operation = read(&entry.path())?;
            if op.version != 1 {
                return Err(Error::Unsupported("journal recovery version".into()));
            }
            result.push(op);
        }
    }
    result.sort_by_key(|op| op.seq);
    Ok(result)
}

fn finish(
    dir: &Path,
    op: &Operation,
    observed: &std::collections::BTreeMap<Vec<u8>, Snapshot>,
) -> Result<()> {
    let mut record = op.record.clone();
    match &mut record {
        Record::Insert {
            path: p,
            dev,
            ino,
            ctime,
            ..
        } => {
            let identity = observed
                .get(p)
                .ok_or_else(|| Error::Stale(path(p).to_owned()))?
                .identity;
            *dev = identity.dev;
            *ino = identity.ino;
            *ctime = identity.ctime;
        }
        Record::Update {
            path: p,
            after,
            identity,
            ..
        } => {
            let current = after.path.as_ref().unwrap_or(p);
            let found = if op.steps.is_empty() {
                let found = Snapshot::required(path(current))?;
                if !identity
                    .is_some_and(|expected| found.identity.matches(&expected, kind_of(&found.kind)))
                {
                    return Err(Error::Stale(path(current).to_owned()));
                }
                found.identity
            } else {
                observed
                    .get(current)
                    .ok_or_else(|| Error::Stale(path(current).to_owned()))?
                    .identity
            };
            *identity = Some(found);
        }
        Record::Delete { .. } => {}
    }
    save(&completed_file(dir, op.seq), &record)?;
    checkpoint("operation-recorded");
    Ok(())
}

fn execute(dir: &Path, op: &Operation, undo: bool) -> Result<()> {
    let steps: Vec<_> = if undo {
        read(&dir.join(format!("inverse-{}.json", op.seq)))?
    } else {
        op.steps.clone()
    };
    for (index, step) in steps.iter().enumerate() {
        let marker = dir.join(format!(
            "{}-{}-{index}",
            if undo { "reverse" } else { "forward" },
            op.seq
        ));
        if marker.exists() {
            continue;
        }
        step.run()?;
        checkpoint("step-applied");
        journal::atomic_write(&marker, b"")?;
        checkpoint("step-recorded");
    }
    // Never certify a replacement merely because all step markers are present.
    let mut final_objects = std::collections::BTreeMap::new();
    for step in &steps {
        match step {
            Step::Move {
                from, to, object, ..
            } => {
                final_objects.insert(from, None);
                final_objects.insert(to, Some(object));
            }
            Step::Attributes { path, after, .. } => {
                final_objects.insert(path, Some(after));
            }
        }
    }
    let mut observed = std::collections::BTreeMap::new();
    for (p, expected) in final_objects {
        let actual = Snapshot::get(path(p))?;
        if !match (expected, actual) {
            (None, None) => true,
            (Some(expected), Some(actual)) if expected.matches(&actual) => {
                observed.insert(p.clone(), actual);
                true
            }
            _ => false,
        } {
            return Err(Error::Stale(path(p).to_owned()));
        }
    }
    if undo {
        journal::atomic_write(&dir.join(format!("undone-{}", op.seq)), b"")
    } else {
        finish(dir, op, &observed)
    }
}

// Tests install a thread-local crash hook; production has no environment-driven
// fault switches. Child-process tests terminate without unwinding at checkpoints.
#[cfg(not(test))]
fn checkpoint(_: &str) {}
#[cfg(test)]
type CrashHook = Box<dyn FnMut(&str)>;
#[cfg(test)]
thread_local! { static CRASH: std::cell::RefCell<Option<CrashHook>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn checkpoint(name: &str) {
    CRASH.with(|c| {
        if let Some(f) = c.borrow_mut().as_mut() {
            f(name);
        }
    });
}

fn stage_dir(journal: &Journal, seq: u64, parent: &Path) -> Result<PathBuf> {
    let stage = parent.join(format!(".fsql-{}-{seq}", journal.id()));
    // A private reserved name contains only preparation artifacts until replay
    // is durable; recovery may discard these without touching public paths.
    if fs::symlink_metadata(&stage).is_ok() {
        return Err(Error::Stale(stage));
    }
    save(
        &journal.dir().join(format!("prepare-{seq}.json")),
        &Preparation {
            stage: bytes(&stage),
            identity: None,
        },
    )?;
    checkpoint("prepare-recorded");
    let fd = open_chain(parent)?;
    let name = c_name(stage.file_name().expect("stage name"), &stage)?;
    rustix::fs::mkdirat(&fd, &name, Mode::from_raw_mode(0o700)).map_err(|e| io(&stage, e))?;
    journal::sync_dir(parent)?;
    checkpoint("stage-created");
    save(
        &journal.dir().join(format!("prepare-{seq}.json")),
        &Preparation {
            stage: bytes(&stage),
            identity: Some(Snapshot::required(&stage)?.identity),
        },
    )?;
    Ok(stage)
}

fn build(
    journal: &Journal,
    seq: u64,
    stage: &Path,
    resolved: &Resolved,
    index: usize,
) -> Result<Operation> {
    let mut steps = Vec::new();
    let (p, record) = match resolved {
        Resolved::Delete(targets) => {
            let target = &targets[index];
            let frozen = &target.frozen;
            let (parent, name) = check(frozen)?;
            if frozen.kind == Kind::Dir && !is_empty_dir(&parent, &name, &frozen.path)? {
                return Err(Error::Plan(format!(
                    "{} is not empty",
                    frozen.path.display()
                )));
            }
            if !matches!(frozen.kind, Kind::File | Kind::Dir | Kind::Symlink) {
                return Err(Error::Unsupported(
                    "journaled deletion of special files".into(),
                ));
            }
            let object = Snapshot::required(&frozen.path)?;
            let tomb = stage.join("old");
            steps.push(Step::Move {
                from: bytes(&frozen.path),
                to: bytes(&tomb),
                object,
                empty: true,
            });
            (
                bytes(&frozen.path),
                Record::Delete {
                    seq,
                    path: bytes(&frozen.path),
                    kind: frozen.kind.as_str().into(),
                    mode: target.before.mode,
                    target: target.before.target.clone(),
                    tomb: Some(bytes(&tomb)),
                    dev: frozen.identity.dev,
                    ino: frozen.identity.ino,
                    ctime: frozen.identity.ctime,
                },
            )
        }
        Resolved::Insert(entries) => {
            let entry = &entries[index];
            if Snapshot::get(&entry.path)?.is_some() {
                return Err(Error::Stale(entry.path.clone()));
            }
            let mut prepared = entry.clone();
            prepared.path = stage.join("new");
            let out = apply_insert(std::slice::from_ref(&prepared), None)?;
            if let Some((_, error)) = out.failures.into_iter().next() {
                return Err(error);
            }
            let object = Snapshot::required(&prepared.path)?;
            let parent = open_chain(stage)?;
            rustix::fs::syncfs(&parent).map_err(|e| io(stage, e))?;
            checkpoint("object-prepared");
            steps.push(Step::Move {
                from: bytes(&prepared.path),
                to: bytes(&entry.path),
                object: object.clone(),
                empty: true,
            });
            (
                bytes(&entry.path),
                Record::Insert {
                    seq,
                    path: bytes(&entry.path),
                    kind: entry.kind.as_str().into(),
                    dev: object.identity.dev,
                    ino: object.identity.ino,
                    ctime: object.identity.ctime,
                },
            )
        }
        Resolved::Update(targets) => {
            let target = &targets[index];
            check(&target.frozen)?;
            let p = &target.frozen.path;
            let original = Snapshot::required(p)?;
            let mut object = original.clone();
            let mut attrs = Attrs::default();
            for change in &target.changes {
                match change {
                    Change::Mode(v) => attrs.mode = Some(*v),
                    Change::Uid(v) => attrs.uid = Some(*v),
                    Change::Gid(v) => attrs.gid = Some(*v),
                    Change::Atime(v) => attrs.atime = Some(*v),
                    Change::Mtime(v) => attrs.mtime = Some(*v),
                    Change::Rename(v) => attrs.path = Some(bytes(v)),
                    Change::Target(v) => attrs.target = Some(v.clone()),
                }
            }
            if attrs.mode.is_some() && object.kind == "symlink" {
                return Err(Error::Plan(format!(
                    "{} is a symlink, its mode cannot change",
                    p.display()
                )));
            }
            if let Some(link) = &attrs.target {
                let new = stage.join("new");
                std::os::unix::fs::symlink(OsStr::from_bytes(link), &new)
                    .map_err(|e| std_io(&new, e))?;
                journal::sync_dir(stage)?;
                let prepared = Snapshot::required(&new)?;
                steps.push(Step::Move {
                    from: bytes(p),
                    to: bytes(&stage.join("old")),
                    object: object.clone(),
                    empty: false,
                });
                steps.push(Step::Move {
                    from: bytes(&new),
                    to: bytes(p),
                    object: prepared.clone(),
                    empty: false,
                });
                object = prepared;
            }
            let mut after = object.clone();
            after.uid = attrs.uid.unwrap_or(after.uid);
            after.gid = attrs.gid.unwrap_or(after.gid);
            if attrs.uid.is_some() || attrs.gid.is_some() {
                after.mode &= !0o6000;
            }
            after.mode = attrs.mode.unwrap_or(after.mode);
            after.atime = attrs.atime.unwrap_or(after.atime);
            after.mtime = attrs.mtime.unwrap_or(after.mtime);
            if !object.matches(&after) || attrs.atime.is_some() {
                steps.push(Step::Attributes {
                    path: bytes(p),
                    before: object,
                    after: after.clone(),
                    atime: attrs.atime.is_some(),
                });
            }
            if let Some(destination) = &attrs.path
                && path(destination) != p
            {
                if Snapshot::get(path(destination))?.is_some() {
                    return Err(Error::Stale(path(destination).to_owned()));
                }
                steps.push(Step::Move {
                    from: bytes(p),
                    to: destination.clone(),
                    object: after,
                    empty: false,
                });
            }
            (
                bytes(p),
                Record::Update {
                    seq,
                    path: bytes(p),
                    kind: original.kind.clone(),
                    before: Attrs {
                        mode: Some(original.mode),
                        uid: Some(original.uid),
                        gid: Some(original.gid),
                        atime: Some(original.atime),
                        mtime: Some(original.mtime),
                        path: Some(bytes(p)),
                        target: original.target,
                    },
                    after: attrs,
                    identity: Some(original.identity),
                },
            )
        }
    };
    let op = Operation {
        version: 1,
        seq,
        path: p,
        stage: bytes(stage),
        stage_identity: Snapshot::required(stage)?.identity,
        steps,
        record,
    };
    save(&operation_file(journal.dir(), seq), &op)?;
    checkpoint("operation-prepared");
    remove_file(&journal.dir().join(format!("prepare-{seq}.json")))?;
    Ok(op)
}

pub(super) fn apply(resolved: &Resolved, journal: &mut Journal) -> Result<Outcome> {
    let _lock = journal::lock(journal.dir())?;
    journal::load(journal.dir().parent().expect("journal base"), journal.id())?;
    if fs::metadata(journal.dir().join("log.jsonl"))
        .map_err(|e| std_io(journal.dir(), e))?
        .len()
        != 0
    {
        return Err(Error::Plan(
            "cannot mix legacy records with resumable mutations".into(),
        ));
    }
    if let Resolved::Delete(targets) | Resolved::Update(targets) = resolved {
        verify_all(targets)?;
    }
    journal::atomic_write(&journal.dir().join("replay-version"), b"1")?;
    let mut order: Vec<usize> = (0..resolved.len()).collect();
    if let Resolved::Delete(targets) = resolved {
        order.sort_by_key(|&i| Reverse(targets[i].frozen.path.components().count()));
    }
    let paths = resolved.paths();
    let mut out = Outcome::default();
    for index in order {
        let p = paths[index].0;
        let seq = journal.reserve_seq();
        let result = (|| {
            // Keep staging outside any directory this statement will remove or
            // insert; undo can then remove newly inserted parents in reverse.
            let mut parent = p
                .parent()
                .ok_or_else(|| Error::Plan("cannot mutate filesystem root".into()))?;
            while paths.iter().any(|(target, _)| parent.starts_with(target)) {
                parent = parent
                    .parent()
                    .ok_or_else(|| Error::Plan("no surviving staging parent".into()))?;
            }
            let local = journal.dir().join("tomb");
            if let Resolved::Update(targets) = resolved
                && !targets[index]
                    .changes
                    .iter()
                    .any(|c| matches!(c, Change::Target(_)))
            {
                parent = &local;
            }
            let stage = stage_dir(journal, seq, parent)?;
            let op = build(journal, seq, &stage, resolved, index)?;
            execute(journal.dir(), &op, false)
        })();
        match result {
            Ok(()) => out.applied += 1,
            Err(error) => {
                out.failures.push((p.to_owned(), error));
                if operation_file(journal.dir(), seq).exists() {
                    out.partial.push(p.to_owned());
                    out.recovery_required.push(journal.dir().to_owned());
                    break;
                }
                // Preparation has never changed a public path.
                if let Err(error) = clean_preparations(journal.dir()) {
                    out.failures.push((p.to_owned(), error));
                    out.recovery_required.push(journal.dir().to_owned());
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn clean_preparations(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| std_io(dir, e))? {
        let entry = entry.map_err(|e| std_io(dir, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("prepare-") || !name.ends_with(".json") {
            continue;
        }
        let seq = name
            .trim_start_matches("prepare-")
            .trim_end_matches(".json");
        if !dir.join(format!("replay-{seq}.json")).exists() {
            let preparation: Preparation = read(&entry.path())?;
            clean_stage(path(&preparation.stage), preparation.identity, None)?;
        }
        remove_file(&entry.path())?;
    }
    Ok(())
}

fn clean_stage(stage: &Path, identity: Option<Identity>, op: Option<&Operation>) -> Result<()> {
    let Some(found) = Snapshot::get(stage)? else {
        return journal::sync_dir(stage.parent().expect("stage parent"));
    };
    if found.kind != "dir" || identity.is_some_and(|i| !found.identity.matches(&i, Kind::Dir)) {
        return Err(Error::Stale(stage.to_owned()));
    }
    let _pinned = open_chain(stage)?;
    let entries = match fs::read_dir(stage) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(std_io(stage, e)),
    };
    // Never recurse. Only reserved preparation names, or matching replay
    // objects, can be removed; an unexpected object remains a visible conflict.
    for entry in entries {
        let entry = entry.map_err(|e| std_io(stage, e))?;
        let p = entry.path();
        let object = Snapshot::required(&p)?;
        let allowed = if let Some(op) = op {
            op.steps.iter().any(|step| match step {
                Step::Move {
                    from,
                    object: expected,
                    ..
                } => path(from) == p && expected.matches(&object),
                _ => false,
            })
        } else {
            identity.is_some() && entry.file_name() == "new"
        };
        if !allowed {
            return Err(Error::Stale(p));
        }
        let (parent, name) = split(&p)?;
        unlink(&parent, &name, kind_of(&object.kind), &p)?;
        journal::sync_dir(stage)?;
        checkpoint("stage-object-removed");
    }
    fs::remove_dir(stage).map_err(|e| std_io(stage, e))?;
    checkpoint("stage-removed");
    journal::sync_dir(stage.parent().expect("stage parent"))
}

pub(super) fn is_replay_journal(base: &Path, id: &str) -> Result<bool> {
    journal::validate_id(id)?;
    Ok(base.join(id).join("replay-version").exists())
}

/// Finish interrupted prepared entries, or resume an interrupted undo. Entries
/// whose preparation never completed are discarded; untouched rows are not run.
/// Conflicting objects are preserved and reported in `Outcome::failures`.
pub fn recover(base: &Path, id: &str) -> Result<Outcome> {
    journal::validate_id(id)?;
    let dir = base.join(id);
    let _lock = journal::lock(&dir)?;
    if dir.join("finished").exists() {
        return Ok(Outcome::default());
    }
    if !is_replay_journal(base, id)? {
        return Err(Error::RecoveryRequired(dir));
    }
    if dir.join("undoing").exists() {
        return undo_locked(base, id);
    }
    recover_locked(&dir)
}

fn recover_locked(dir: &Path) -> Result<Outcome> {
    clean_preparations(dir)?;
    let mut out = Outcome::default();
    for op in operations(dir)? {
        if completed_file(dir, op.seq).exists() {
            continue;
        }
        match execute(dir, &op, false) {
            Ok(()) => out.applied += 1,
            Err(e) => {
                out.failures.push((path(&op.path).to_owned(), e));
                out.recovery_required.push(dir.to_owned());
                break;
            }
        }
    }
    Ok(out)
}

pub(super) fn undo(base: &Path, id: &str) -> Result<Outcome> {
    journal::validate_id(id)?;
    let dir = base.join(id);
    let _lock = journal::lock(&dir)?;
    if dir.join("finished").exists() {
        return Ok(Outcome::default());
    }
    if dir.join("undoing").exists() {
        return undo_locked(base, id);
    }
    clean_preparations(&dir)?;
    journal::atomic_write(&dir.join("undoing"), b"")?;
    checkpoint("undo-started");
    undo_locked(base, id)
}

fn verify_undo(dir: &Path, op: &Operation) -> Result<()> {
    let record: Record = read(&completed_file(dir, op.seq))?;
    let (p, expected, kind) = match record {
        Record::Insert {
            path,
            dev,
            ino,
            ctime,
            kind,
            ..
        } => (path, Some(Identity { dev, ino, ctime }), kind),
        Record::Update {
            path,
            after,
            identity,
            kind,
            ..
        } => (after.path.unwrap_or(path), identity, kind),
        Record::Delete { .. } => return Ok(()),
    };
    let current = Snapshot::required(path(&p))?;
    if !expected.is_some_and(|e| current.identity.matches(&e, kind_of(&kind))) {
        return Err(Error::Stale(path(&p).to_owned()));
    }
    Ok(())
}

/// Persist exactly the applied prefix, including a possibly interrupted step.
/// Undo must not require a denied or otherwise impossible forward step to run.
fn inverse_steps(dir: &Path, op: &Operation) -> Result<Vec<Step>> {
    if completed_file(dir, op.seq).exists() {
        verify_undo(dir, op)?;
        return Ok(op.steps.iter().rev().map(Step::inverse).collect());
    }
    let mut applied = Vec::new();
    for (index, step) in op.steps.iter().enumerate() {
        if dir.join(format!("forward-{}-{index}", op.seq)).exists() {
            applied.push(step.inverse());
            continue;
        }
        match step {
            Step::Move {
                from, to, object, ..
            } => {
                let source = Snapshot::get(path(from))?;
                if source.as_ref().is_some_and(|found| object.matches(found)) {
                    // A refused rename has not moved our source; an unrelated
                    // destination requires no inverse and remains untouched.
                } else if source.is_none()
                    && Snapshot::get(path(to))?.is_some_and(|found| object.matches(&found))
                {
                    applied.push(step.inverse());
                } else {
                    return Err(Error::Stale(path(&op.path).to_owned()));
                }
            }
            Step::Attributes {
                path: p,
                before,
                atime,
                ..
            } => {
                let current = Snapshot::required(path(p))?;
                if !before.matches(&current) || (*atime && before.atime != current.atime) {
                    // The inverse validates every field against the before and
                    // after images before touching the pinned object.
                    applied.push(step.inverse());
                }
            }
        }
        break;
    }
    applied.reverse();
    Ok(applied)
}

fn undo_locked(base: &Path, id: &str) -> Result<Outcome> {
    let dir = base.join(id);
    let mut out = Outcome::default();
    for op in operations(&dir)?.into_iter().rev() {
        let undone = dir.join(format!("undone-{}", op.seq));
        let result = (|| {
            if !undone.exists() {
                let started = dir.join(format!("inverse-{}.json", op.seq));
                if !started.exists() {
                    save(&started, &inverse_steps(&dir, &op)?)?;
                    checkpoint("inverse-prepared");
                }
                execute(&dir, &op, true)?;
                checkpoint("undo-recorded");
            }
            clean_stage(path(&op.stage), Some(op.stage_identity), Some(&op))
        })();
        match result {
            Ok(()) => out.applied += 1,
            Err(e) => {
                out.failures.push((path(&op.path).to_owned(), e));
                out.recovery_required.push(dir.clone());
                return Ok(out);
            }
        }
    }
    // Retain completion records: removing a live journal recursively can itself
    // be interrupted. An atomically written terminal marker is unambiguous.
    journal::atomic_write(&dir.join("finished"), b"")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::WalkOptions;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "fsql-replay-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("tree/dir")).unwrap();
            fs::write(root.join("tree/file"), b"original bytes").unwrap();
            fs::set_permissions(root.join("tree/file"), fs::Permissions::from_mode(0o640)).unwrap();
            std::os::unix::fs::symlink("file", root.join("tree/link")).unwrap();
            Self(root)
        }
        fn tree(&self) -> PathBuf {
            self.0.join("tree")
        }
        fn base(&self) -> PathBuf {
            self.0.join("journal")
        }
        fn id(&self) -> String {
            fs::read_dir(self.base())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name()
                .into_string()
                .unwrap()
        }
        fn sql(&self, case: &str) -> String {
            match case {
                "insert" => format!("insert into files(path, content, mode) values ('{}/new', 'new bytes', 0o600)", self.tree().display()),
                "insert-dir" => format!("insert into files(path, kind) values ('{0}/new', 'dir'), ('{0}/new/child', 'file')", self.tree().display()),
                "delete" => "delete from files where name = 'file'".into(),
                "delete-dir" => "delete from files where name = 'dir'".into(),
                "delete-link" => "delete from files where name = 'link'".into(),
                "update" => "update files set mode = 0o600, mtime = '2020-01-02T03:04:05Z', name = 'renamed' where name = 'file'".into(),
                "update-link" => "update files set target = 'new-target', name = 'renamed-link' where name = 'link'".into(),
                _ => panic!("case"),
            }
        }
        fn original(&self) {
            assert_eq!(
                fs::read(self.tree().join("file")).unwrap(),
                b"original bytes"
            );
            assert_eq!(
                fs::metadata(self.tree().join("file")).unwrap().mode() & 0o777,
                0o640
            );
            assert_eq!(
                fs::read_link(self.tree().join("link")).unwrap(),
                Path::new("file")
            );
            assert!(self.tree().join("dir").is_dir());
            for name in ["new", "renamed", "renamed-link"] {
                assert!(
                    fs::symlink_metadata(self.tree().join(name)).is_err(),
                    "{name}"
                );
            }
            assert_eq!(
                fs::read_dir(self.tree()).unwrap().count(),
                3,
                "staging artifacts remain"
            );
        }
        fn child(&self, action: &str, case: &str, crash: usize) -> bool {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mutate::recovery::tests::crash_worker",
                    "--nocapture",
                ])
                .env("FSQL_TEST_ROOT", &self.0)
                .env("FSQL_TEST_ACTION", action)
                .env("FSQL_TEST_CASE", case)
                .env("FSQL_TEST_CRASH", crash.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success() || output.status.code() == Some(86),
                "{action}/{case}/{crash}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output.status.success()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn crash_worker() {
        let Some(root) = std::env::var_os("FSQL_TEST_ROOT") else {
            return;
        };
        let fx = Fixture(PathBuf::from(root));
        let mut remaining: usize = std::env::var("FSQL_TEST_CRASH").unwrap().parse().unwrap();
        CRASH.with(|c| {
            *c.borrow_mut() = Some(Box::new(move |_| {
                if remaining == 0 {
                    std::process::exit(86);
                }
                remaining -= 1;
            }))
        });
        let action = std::env::var("FSQL_TEST_ACTION").unwrap();
        let case = std::env::var("FSQL_TEST_CASE").unwrap();
        let outcome = match action.as_str() {
            "apply" => {
                let planner = Planner::new(fx.tree(), WalkOptions::default());
                let sql = fx.sql(&case);
                let resolved = resolve(
                    &planner
                        .plan(&sql)
                        .unwrap()
                        .into_iter()
                        .next()
                        .expect("one plan"),
                    &planner,
                    &mut |_| {},
                )
                .unwrap();
                let mut log = Journal::open(&fx.base(), &sql).unwrap();
                apply(&resolved, &mut log).unwrap()
            }
            "undo" => undo(&fx.base(), &fx.id()).unwrap(),
            "recover" => recover(&fx.base(), &fx.id()).unwrap(),
            _ => panic!("action"),
        };
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        CRASH.with(|c| *c.borrow_mut() = None);
        std::mem::forget(fx);
    }

    const CASES: &[&str] = &[
        "insert",
        "insert-dir",
        "delete",
        "delete-dir",
        "delete-link",
        "update",
        "update-link",
    ];

    #[test]
    fn process_death_at_every_apply_checkpoint_is_recoverable() {
        for case in CASES {
            let mut reached_end = false;
            for crash in 0..64 {
                let fx = Fixture::new();
                let completed = fx.child("apply", case, crash);
                let id = fx.id();
                let recovered = recover(&fx.base(), &id).unwrap();
                assert!(
                    recovered.failures.is_empty(),
                    "{case}/{crash}: {:?}",
                    recovered.failures
                );
                assert!(recover(&fx.base(), &id).unwrap().failures.is_empty());
                if *case == "insert" && fx.tree().join("new").exists() {
                    assert_eq!(fs::read(fx.tree().join("new")).unwrap(), b"new bytes");
                }
                let out = undo(&fx.base(), &id).unwrap();
                assert!(
                    out.failures.is_empty(),
                    "{case}/{crash}: {:?}",
                    out.failures
                );
                fx.original();
                assert!(journal::list(&fx.base()).unwrap().is_empty());
                if completed {
                    reached_end = true;
                    break;
                }
            }
            assert!(reached_end, "unbounded apply checkpoints: {case}");
        }
    }

    #[test]
    fn unfinished_apply_can_be_undone_without_finishing_it() {
        for case in CASES {
            let mut reached_end = false;
            for crash in 0..64 {
                let fx = Fixture::new();
                let completed = fx.child("apply", case, crash);
                let out = undo(&fx.base(), &fx.id()).unwrap();
                assert!(
                    out.failures.is_empty(),
                    "{case}/{crash}: {:?}",
                    out.failures
                );
                fx.original();
                if completed {
                    reached_end = true;
                    break;
                }
            }
            assert!(reached_end, "unbounded apply checkpoints");
        }
    }

    #[test]
    fn denied_forward_step_can_be_rolled_back() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let fx = Fixture::new();
        let planner = Planner::new(fx.tree(), WalkOptions::default());
        let sql = "update files set target = 'new-target', uid = 0 where name = 'link'";
        let resolved = resolve(
            &planner
                .plan(sql)
                .unwrap()
                .into_iter()
                .next()
                .expect("one plan"),
            &planner,
            &mut |_| {},
        )
        .unwrap();
        let mut journal = Journal::open(&fx.base(), sql).unwrap();
        let out = apply(&resolved, &mut journal).unwrap();
        assert_eq!(out.failures.len(), 1);
        assert_eq!(
            fs::read_link(fx.tree().join("link")).unwrap(),
            Path::new("new-target")
        );
        let out = undo(&fx.base(), journal.id()).unwrap();
        assert!(out.failures.is_empty(), "{:?}", out.failures);
        fx.original();
    }

    #[test]
    fn process_death_at_every_undo_checkpoint_is_recoverable() {
        for case in CASES {
            let mut reached_end = false;
            for crash in 0..64 {
                let fx = Fixture::new();
                assert!(fx.child("apply", case, usize::MAX));
                let completed = fx.child("undo", case, crash);
                let id = fx.id();
                let out = recover(&fx.base(), &id).unwrap();
                assert!(
                    out.failures.is_empty(),
                    "{case}/{crash}: {:?}",
                    out.failures
                );
                fx.original();
                assert_eq!(recover(&fx.base(), &id).unwrap().applied, 0);
                assert_eq!(undo(&fx.base(), &id).unwrap().applied, 0);
                if completed {
                    reached_end = true;
                    break;
                }
            }
            assert!(reached_end, "unbounded undo checkpoints: {case}");
        }
    }

    #[test]
    fn recovery_can_itself_be_interrupted_and_resumed() {
        for crash in 0..32 {
            let fx = Fixture::new();
            // update-link: prepare, mkdir, durable operation, then first rename.
            assert!(!fx.child("apply", "update-link", 3));
            let completed = fx.child("recover", "update-link", crash);
            let id = fx.id();
            let out = recover(&fx.base(), &id).unwrap();
            assert!(out.failures.is_empty(), "{crash}: {:?}", out.failures);
            let out = undo(&fx.base(), &id).unwrap();
            assert!(out.failures.is_empty(), "{:?}", out.failures);
            fx.original();
            if completed {
                return;
            }
        }
        panic!("unbounded recovery checkpoints");
    }

    #[test]
    fn replacement_after_last_step_is_never_certified_or_removed() {
        let fx = Fixture::new();
        assert!(fx.child("apply", "insert", usize::MAX));
        let id = fx.id();
        let dir = fx.base().join(&id);
        fs::remove_file(completed_file(&dir, 0)).unwrap();
        let original = fx.tree().join("new");
        fs::rename(&original, fx.tree().join("saved")).unwrap();
        fs::write(&original, b"replacement").unwrap();
        let out = recover(&fx.base(), &id).unwrap();
        assert_eq!(out.failures.len(), 1);
        assert!(!completed_file(&dir, 0).exists());
        assert_eq!(fs::read(&original).unwrap(), b"replacement");
        fs::remove_file(&original).unwrap();
        fs::rename(fx.tree().join("saved"), &original).unwrap();
        assert!(recover(&fx.base(), &id).unwrap().failures.is_empty());
        assert!(undo(&fx.base(), &id).unwrap().failures.is_empty());
        fx.original();
    }

    #[test]
    fn undo_conflict_is_retryable_without_touching_either_object() {
        let fx = Fixture::new();
        assert!(fx.child("apply", "delete", usize::MAX));
        let id = fx.id();
        fs::write(fx.tree().join("file"), b"replacement").unwrap();
        let out = undo(&fx.base(), &id).unwrap();
        assert_eq!(out.failures.len(), 1);
        assert_eq!(fs::read(fx.tree().join("file")).unwrap(), b"replacement");
        fs::rename(fx.tree().join("file"), fx.0.join("conflict")).unwrap();
        assert!(recover(&fx.base(), &id).unwrap().failures.is_empty());
        fx.original();
        assert_eq!(fs::read(fx.0.join("conflict")).unwrap(), b"replacement");
    }

    #[test]
    fn undo_reverts_prior_steps_when_a_rename_destination_becomes_occupied() {
        let fx = Fixture::new();
        let planner = Planner::new(fx.tree(), WalkOptions::default());
        let sql = "update files set mode = 0o600, name = 'renamed' where name = 'file'";
        let resolved = resolve(
            &planner
                .plan(sql)
                .unwrap()
                .into_iter()
                .next()
                .expect("one plan"),
            &planner,
            &mut |_| {},
        )
        .unwrap();
        let destination = fx.tree().join("renamed");
        let other = destination.clone();
        CRASH.with(|c| {
            *c.borrow_mut() = Some(Box::new(move |name| {
                if name == "step-recorded" {
                    fs::write(&other, b"unrelated").unwrap();
                }
            }))
        });
        let mut log = Journal::open(&fx.base(), sql).unwrap();
        let out = apply(&resolved, &mut log).unwrap();
        CRASH.with(|c| *c.borrow_mut() = None);
        assert_eq!(out.failures.len(), 1);
        assert_eq!(
            fs::metadata(fx.tree().join("file")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(undo(&fx.base(), log.id()).unwrap().failures.is_empty());
        assert_eq!(fs::read(&destination).unwrap(), b"unrelated");
        fs::remove_file(destination).unwrap();
        fx.original();
    }

    #[test]
    fn private_tombstones_do_not_reappear_in_queries() {
        let fx = Fixture::new();
        assert!(fx.child("apply", "delete", usize::MAX));
        let engine = crate::Engine::new(fx.tree(), WalkOptions::default());
        let (rows, _) = engine
            .prepare_query("select count(*) from files where kind = 'file'")
            .unwrap()
            .collect()
            .unwrap();
        assert_eq!(rows.rows[0][0], Value::Int(0));
        let ops = operations(&fx.base().join(fx.id())).unwrap();
        assert!(Walker::new(path(&ops[0].stage), WalkOptions::default()).is_err());
        let reserved = format!(
            "insert into files(path) values ('{}/.fsql-abc-def-0')",
            fx.tree().display()
        );
        assert!(
            engine
                .resolve_mutation(&reserved)
                .unwrap()
                .apply(&fx.base())
                .is_err()
        );
    }

    #[test]
    fn torn_temporary_checkpoints_are_ignored_and_replaced() {
        let fx = Fixture::new();
        assert!(!fx.child("apply", "insert", 4));
        let id = fx.id();
        let dir = fx.base().join(&id);
        fs::write(dir.join("complete-0.tmp"), b"{torn").unwrap();
        fs::write(dir.join("prepare-0.tmp"), b"{torn").unwrap();
        assert!(recover(&fx.base(), &id).unwrap().failures.is_empty());
        assert_eq!(journal::load(&fx.base(), &id).unwrap().len(), 1);
        assert!(undo(&fx.base(), &id).unwrap().failures.is_empty());
        fx.original();
    }

    #[test]
    fn recovery_paths_preserve_non_utf8_filenames() {
        let fx = Fixture::new();
        let original = fx.tree().join(OsStr::from_bytes(b"bytes-\xff"));
        fs::rename(fx.tree().join("file"), &original).unwrap();
        let planner = Planner::new(fx.tree(), WalkOptions::default());
        let sql = "delete from files where kind = 'file'";
        let resolved = resolve(
            &planner
                .plan(sql)
                .unwrap()
                .into_iter()
                .next()
                .expect("one plan"),
            &planner,
            &mut |_| {},
        )
        .unwrap();
        let mut log = Journal::open(&fx.base(), sql).unwrap();
        assert!(apply(&resolved, &mut log).unwrap().failures.is_empty());
        assert!(recover(&fx.base(), log.id()).unwrap().failures.is_empty());
        assert!(undo(&fx.base(), log.id()).unwrap().failures.is_empty());
        assert_eq!(fs::read(original).unwrap(), b"original bytes");
    }

    #[test]
    fn journal_lock_excludes_concurrent_recovery() {
        let fx = Fixture::new();
        assert!(fx.child("apply", "insert", usize::MAX));
        let id = fx.id();
        let lock = journal::lock(&fx.base().join(&id)).unwrap();
        assert!(recover(&fx.base(), &id).is_err());
        assert!(undo(&fx.base(), &id).is_err());
        drop(lock);
        assert!(undo(&fx.base(), &id).unwrap().failures.is_empty());
    }
}
