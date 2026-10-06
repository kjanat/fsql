use std::cmp::Reverse;
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, Dir, Gid, Mode, OFlags, RenameFlags, Timespec, Timestamps, UTIME_OMIT, Uid,
    XattrFlags,
};
use rustix::io::Errno;

use crate::column::Column;
use crate::error::{Error, Result};
use crate::eval::{Evaluator, Row, render};
use crate::exec::{self, Aliased, compare_keys};
use crate::journal::{self, Attrs, Journal, Record};
use crate::plan::{self, InsertColumn, InsertPlan, InsertRows, OrderKey, Plan, Planner, Set};
use crate::row::{Entry, Frozen, Identity, Kind, STATX_MASK};
use crate::time;
use crate::value::Value;
use crate::walk::{Walker, c_name, open_chain, validate_path};
use crate::xattr;

mod recovery;
pub use recovery::recover;

#[derive(Debug, Clone)]
pub struct Before {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: i64,
    pub mtime: i64,
    pub size: u64,
    pub target: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum Change {
    Mode(u32),
    Uid(u32),
    Gid(u32),
    Atime(i64),
    Mtime(i64),
    Rename(PathBuf),
    Target(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct Target {
    pub frozen: Frozen,
    pub before: Before,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub path: PathBuf,
    pub kind: Kind,
    pub mode: Option<u32>,
    pub target: Option<Vec<u8>>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<i64>,
    pub mtime: Option<i64>,
    pub source: Option<PathBuf>,
    source_identity: Option<Identity>,
    pub content: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum Resolved {
    Delete(Vec<Target>),
    Update(Vec<Target>),
    Insert(Vec<NewEntry>),
}

impl Resolved {
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Delete(_) => "DELETE",
            Self::Update(_) => "UPDATE",
            Self::Insert(_) => "INSERT",
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Delete(t) | Self::Update(t) => t.len(),
            Self::Insert(e) => e.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> u64 {
        match self {
            Self::Delete(t) | Self::Update(t) => t.iter().map(|t| t.before.size).sum(),
            Self::Insert(e) => e
                .iter()
                .map(|e| e.content.as_ref().map(|c| c.len() as u64).unwrap_or(0))
                .sum(),
        }
    }

    pub fn paths(&self) -> Vec<(&Path, u64)> {
        match self {
            Self::Delete(t) | Self::Update(t) => t
                .iter()
                .map(|t| (t.frozen.path.as_path(), t.before.size))
                .collect(),
            Self::Insert(e) => e.iter().map(|e| (e.path.as_path(), 0)).collect(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub applied: usize,
    pub failures: Vec<(PathBuf, Error)>,
    /// Entries changed before a later step failed.
    pub partial: Vec<PathBuf>,
    /// Journals retaining unfinished durable intents.
    pub recovery_required: Vec<PathBuf>,
}

fn io(path: &Path, errno: Errno) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from(errno),
    }
}

fn std_io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn skippable(_error: &Error) -> bool {
    false // A mutation must never resolve from an incomplete scan.
}

pub fn resolve(plan: &Plan, planner: &Planner, errors: &mut dyn FnMut(Error)) -> Result<Resolved> {
    resolve_with_options(
        plan,
        planner,
        crate::execution::ExecutionOptions::default(),
        errors,
    )
}

pub fn resolve_with_options(
    plan: &Plan,
    planner: &Planner,
    mut options: crate::execution::ExecutionOptions,
    errors: &mut dyn FnMut(Error),
) -> Result<Resolved> {
    crate::bind::plan(plan, planner)?;
    options.error_policy = crate::execution::ErrorPolicy::Strict;
    match plan {
        Plan::Select(_) => Err(Error::Plan("SELECT is not a mutation".to_owned())),
        Plan::Delete(delete) => Ok(Resolved::Delete(collect(
            &Selection {
                source: &delete.source,
                alias: &delete.alias,
                filter: &delete.filter,
                assignments: &[],
                order_by: &delete.order_by,
                limit: delete.limit,
            },
            planner,
            options,
            errors,
        )?)),
        Plan::Update(update) => Ok(Resolved::Update(collect(
            &Selection {
                source: &update.source,
                alias: &update.alias,
                filter: &update.filter,
                assignments: &update.assignments,
                order_by: &update.order_by,
                limit: update.limit,
            },
            planner,
            options,
            errors,
        )?)),
        Plan::Insert(insert) => Ok(Resolved::Insert(new_entries(
            insert, planner, options, errors,
        )?)),
    }
}

struct Selection<'a> {
    source: &'a plan::Source,
    alias: &'a str,
    filter: &'a sqlparser::ast::Expr,
    assignments: &'a [Set],
    order_by: &'a [OrderKey],
    limit: Option<usize>,
}

fn collect(
    selection: &Selection<'_>,
    planner: &Planner,
    options: crate::execution::ExecutionOptions,
    errors: &mut dyn FnMut(Error),
) -> Result<Vec<Target>> {
    let (mut evaluator, ctx) = exec::standalone_with_options(planner, options);
    let walker = Walker::cancellable(
        &selection.source.root,
        selection.source.options.clone(),
        &ctx.control.options.cancellation,
    )?;
    let mut collected: Vec<(Vec<Value>, Target)> = Vec::new();
    for entry in walker {
        ctx.control.step()?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if skippable(&error) => {
                errors(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let row = Aliased {
            alias: selection.alias,
            row: &entry,
        };
        let step = (|| -> Result<Option<(Vec<Value>, Target)>> {
            if evaluator.eval(selection.filter, &row)?.truth() != Some(true) {
                return Ok(None);
            }
            let before = before(&entry)?;
            let frozen = entry.freeze()?;
            let changes = selection
                .assignments
                .iter()
                .map(|set| change(&mut evaluator, set, &row, &frozen))
                .collect::<Result<Vec<Change>>>()?;
            let keys = selection
                .order_by
                .iter()
                .map(|key| evaluator.eval(&key.expr, &row))
                .collect::<Result<Vec<Value>>>()?;
            Ok(Some((
                keys,
                Target {
                    frozen,
                    before,
                    changes,
                },
            )))
        })();
        match step {
            Ok(Some(item)) => {
                ctx.control.retain(
                    std::mem::size_of::<Target>()
                        .saturating_add(item.1.frozen.path.as_os_str().len())
                        .saturating_add(crate::execution::row_bytes(&item.0)),
                )?;
                collected.push(item);
            }
            Ok(None) => {}
            Err(error) if skippable(&error) => errors(error),
            Err(error) => return Err(error),
        }
    }
    exec::drain(&ctx, errors);
    if !selection.order_by.is_empty() {
        collected.sort_by(|a, b| compare_keys(&a.0, &b.0, selection.order_by));
    }
    let mut targets: Vec<Target> = collected.into_iter().map(|(_, target)| target).collect();
    if let Some(limit) = selection.limit {
        targets.truncate(limit);
    }
    Ok(targets)
}

fn int(value: &Value, what: &str) -> Result<i64> {
    match value {
        Value::Int(n) => Ok(*n),
        other => Err(Error::TypeMismatch {
            operation: format!("set {what}"),
            left: "int".to_owned(),
            right: render(other),
        }),
    }
}

fn nanos(value: &Value, what: &str) -> Result<i64> {
    match value {
        Value::Timestamp(t) => Ok(t.0),
        Value::Int(n) => Ok(*n),
        Value::Text(text) => time::parse_iso(text)
            .map(|t| t.0)
            .ok_or_else(|| Error::InvalidTimestamp(text.clone())),
        other => Err(Error::TypeMismatch {
            operation: format!("set {what}"),
            left: "timestamp".to_owned(),
            right: render(other),
        }),
    }
}

fn bytes(value: &Value, what: &str) -> Result<Vec<u8>> {
    match value {
        Value::Text(s) => Ok(s.as_bytes().to_vec()),
        Value::Blob(b) => Ok(b.clone()),
        other => Err(Error::TypeMismatch {
            operation: format!("set {what}"),
            left: "text".to_owned(),
            right: render(other),
        }),
    }
}

fn mode_bits(value: &Value) -> Result<u32> {
    let mode = int(value, "mode")?;
    if !(0..=0o7777).contains(&mode) {
        return Err(Error::Plan(format!("mode {mode:o} is out of range")));
    }
    Ok(mode as u32)
}

fn id_of(value: &Value, what: &str) -> Result<u32> {
    u32::try_from(int(value, what)?).map_err(|_| Error::Plan(format!("{what} out of range")))
}

fn before(entry: &Entry) -> Result<Before> {
    let stat = entry.stat()?;
    let target = match entry.column("target")? {
        Value::Text(s) => Some(s.into_bytes()),
        Value::Blob(b) => Some(b),
        _ => None,
    };
    Ok(Before {
        mode: u32::from(stat.stx_mode),
        uid: stat.stx_uid,
        gid: stat.stx_gid,
        atime: time::from_parts(stat.stx_atime.tv_sec, stat.stx_atime.tv_nsec).0,
        mtime: time::from_parts(stat.stx_mtime.tv_sec, stat.stx_mtime.tv_nsec).0,
        size: stat.stx_size,
        target,
    })
}

fn user_id(name: &[u8]) -> Result<u32> {
    uzers::get_user_by_name(OsStr::from_bytes(name))
        .map(|user| user.uid())
        .ok_or_else(|| Error::Plan(format!("unknown user `{}`", String::from_utf8_lossy(name))))
}

fn group_id(name: &[u8]) -> Result<u32> {
    uzers::get_group_by_name(OsStr::from_bytes(name))
        .map(|group| group.gid())
        .ok_or_else(|| Error::Plan(format!("unknown group `{}`", String::from_utf8_lossy(name))))
}

fn change(evaluator: &mut Evaluator, set: &Set, row: &dyn Row, frozen: &Frozen) -> Result<Change> {
    let value = evaluator.eval(&set.value, row)?;
    if value.is_null() {
        return Err(Error::Plan(format!(
            "cannot assign NULL to `{}`",
            set.column.name()
        )));
    }
    Ok(match set.column {
        Column::Mode => Change::Mode(mode_bits(&value)?),
        Column::Uid => Change::Uid(id_of(&value, "uid")?),
        Column::Gid => Change::Gid(id_of(&value, "gid")?),
        Column::User => Change::Uid(user_id(&bytes(&value, "user")?)?),
        Column::Group => Change::Gid(group_id(&bytes(&value, "group")?)?),
        Column::Atime => Change::Atime(nanos(&value, "atime")?),
        Column::Mtime => Change::Mtime(nanos(&value, "mtime")?),
        Column::Path => {
            let path = PathBuf::from(OsStr::from_bytes(&bytes(&value, "path")?));
            if !path.is_absolute() {
                return Err(Error::Plan(format!(
                    "new path `{}` must be absolute",
                    path.display()
                )));
            }
            validate_path(&path)?;
            Change::Rename(path)
        }
        Column::Name => {
            let name = bytes(&value, "name")?;
            if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
                return Err(Error::Plan(format!(
                    "`{}` is not a valid file name",
                    String::from_utf8_lossy(&name)
                )));
            }
            Change::Rename(frozen.parent.join(OsStr::from_bytes(&name)))
        }
        Column::Parent => {
            let parent = PathBuf::from(OsStr::from_bytes(&bytes(&value, "parent")?));
            if !parent.is_absolute() {
                return Err(Error::Plan(format!(
                    "new parent `{}` must be absolute",
                    parent.display()
                )));
            }
            validate_path(&parent)?;
            Change::Rename(parent.join(&frozen.name))
        }
        Column::Target => {
            if frozen.kind != Kind::Symlink {
                return Err(Error::Plan(format!(
                    "{} is not a symlink, cannot set `target`",
                    frozen.path.display()
                )));
            }
            Change::Target(bytes(&value, "target")?)
        }
        other => {
            return Err(Error::Plan(format!(
                "column `{}` cannot be assigned",
                other.name()
            )));
        }
    })
}

fn new_entries(
    plan: &InsertPlan,
    planner: &Planner,
    options: crate::execution::ExecutionOptions,
    _errors: &mut dyn FnMut(Error),
) -> Result<Vec<NewEntry>> {
    let control = crate::execution::Control::new(options.clone());
    let rows: Vec<Vec<Value>> = match &plan.rows {
        InsertRows::Values(rows) => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(plan::constant)
                    .collect::<Result<Vec<Value>>>()
            })
            .collect::<Result<Vec<_>>>()?,
        InsertRows::Query(query) => {
            let (set, _) = exec::run_with_options(query, planner, options)?;
            if set.headers.len() != plan.columns.len() {
                return Err(Error::Plan(format!(
                    "INSERT names {} columns but SELECT yields {}",
                    plan.columns.len(),
                    set.headers.len()
                )));
            }
            set.rows
        }
    };
    rows.iter()
        .map(|row| {
            control.row(row)?;
            new_entry(plan, row)
        })
        .collect()
}

fn new_entry(plan: &InsertPlan, row: &[Value]) -> Result<NewEntry> {
    let mut entry = NewEntry {
        path: PathBuf::new(),
        kind: Kind::File,
        mode: None,
        target: None,
        uid: None,
        gid: None,
        atime: None,
        mtime: None,
        source: None,
        source_identity: None,
        content: None,
    };
    let mut kind_given = false;
    for (column, value) in plan.columns.iter().zip(row) {
        if value.is_null() {
            if *column == InsertColumn::Column(Column::Path) {
                return Err(Error::Plan("INSERT path is NULL".to_owned()));
            }
            continue;
        }
        match column {
            InsertColumn::Column(Column::Path) => {
                entry.path = PathBuf::from(OsStr::from_bytes(&bytes(value, "path")?));
            }
            InsertColumn::Column(Column::Kind) => {
                kind_given = true;
                entry.kind = match render(value).to_ascii_lowercase().as_str() {
                    "file" => Kind::File,
                    "dir" | "directory" => Kind::Dir,
                    "symlink" | "link" => Kind::Symlink,
                    other => return Err(Error::Plan(format!("cannot create a `{other}`"))),
                }
            }
            InsertColumn::Column(Column::Mode) => entry.mode = Some(mode_bits(value)?),
            InsertColumn::Column(Column::Target) => entry.target = Some(bytes(value, "target")?),
            InsertColumn::Column(Column::Uid) => entry.uid = Some(id_of(value, "uid")?),
            InsertColumn::Column(Column::Gid) => entry.gid = Some(id_of(value, "gid")?),
            InsertColumn::Column(Column::User) => {
                entry.uid = Some(user_id(&bytes(value, "user")?)?)
            }
            InsertColumn::Column(Column::Group) => {
                entry.gid = Some(group_id(&bytes(value, "group")?)?)
            }
            InsertColumn::Column(Column::Atime) => entry.atime = Some(nanos(value, "atime")?),
            InsertColumn::Column(Column::Mtime) => entry.mtime = Some(nanos(value, "mtime")?),
            InsertColumn::Source => {
                entry.source = Some(PathBuf::from(OsStr::from_bytes(&bytes(value, "source")?)));
            }
            InsertColumn::Content => entry.content = Some(bytes(value, "content")?),
            InsertColumn::Column(other) => {
                return Err(Error::Plan(format!(
                    "column `{}` cannot be set by INSERT",
                    other.name()
                )));
            }
        }
    }
    if !entry.path.is_absolute() {
        return Err(Error::Plan(format!(
            "INSERT path `{}` must be absolute",
            entry.path.display()
        )));
    }
    validate_path(&entry.path)?;
    if let Some(source) = &entry.source {
        validate_path(source)?;
        if !source.is_absolute() {
            return Err(Error::Plan(format!(
                "INSERT source `{}` must be absolute",
                source.display()
            )));
        }
        let stat = rustix::fs::statx(CWD, source, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK)
            .map_err(|e| io(source, e))?;
        entry.source_identity = Some(Identity::of(&stat));
        let source_kind = Kind::from_mode(u32::from(stat.stx_mode));
        if !kind_given {
            entry.kind = source_kind;
        } else if entry.kind != source_kind {
            return Err(Error::Plan(format!(
                "INSERT kind `{}` differs from source `{}` which is a {}",
                entry.kind.as_str(),
                source.display(),
                source_kind.as_str()
            )));
        }
        if entry.mode.is_none() {
            entry.mode = Some(u32::from(stat.stx_mode) & 0o7777);
        }
        if entry.kind == Kind::Symlink && entry.target.is_none() {
            let target =
                rustix::fs::readlinkat(CWD, source, Vec::new()).map_err(|e| io(source, e))?;
            entry.target = Some(target.into_bytes());
        }
    }
    if entry.content.is_some() && entry.kind != Kind::File {
        return Err(Error::Plan(format!(
            "`content` applies to files, `{}` is a {}",
            entry.path.display(),
            entry.kind.as_str()
        )));
    }
    if entry.kind == Kind::Symlink && entry.target.is_none() {
        return Err(Error::Plan(format!(
            "symlink `{}` needs a `target`",
            entry.path.display()
        )));
    }
    if !matches!(entry.kind, Kind::File | Kind::Dir | Kind::Symlink) {
        return Err(Error::Plan(format!(
            "cannot create a {}",
            entry.kind.as_str()
        )));
    }
    Ok(entry)
}

fn check(frozen: &Frozen) -> Result<(OwnedFd, CString)> {
    let dirfd = open_chain(&frozen.parent)?;
    let name = c_name(&frozen.name, &frozen.path)?;
    let stat = rustix::fs::statx(&dirfd, &name, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK)
        .map_err(|e| io(&frozen.path, e))?;
    if !Identity::of(&stat).matches(&frozen.identity, frozen.kind) {
        return Err(Error::Stale(frozen.path.clone()));
    }
    Ok((dirfd, name))
}

fn verify_all(targets: &[Target]) -> Result<()> {
    for target in targets {
        check(&target.frozen)?;
    }
    Ok(())
}

pub fn apply(resolved: &Resolved, journal: Option<&mut Journal>) -> Result<Outcome> {
    let mut paths: Vec<&Path> = resolved.paths().into_iter().map(|(p, _)| p).collect();
    if let Resolved::Update(targets) = resolved {
        paths.extend(targets.iter().flat_map(|t| {
            t.changes.iter().filter_map(|c| {
                if let Change::Rename(p) = c {
                    Some(p.as_path())
                } else {
                    None
                }
            })
        }));
    }
    if paths.into_iter().any(crate::walk::recovery_path) {
        return Err(Error::Plan(
            "private recovery paths are reserved; use recover or undo".into(),
        ));
    }
    if let Some(journal) = journal {
        return recovery::apply(resolved, journal);
    }
    match resolved {
        Resolved::Delete(targets) => apply_delete(targets, None),
        Resolved::Update(targets) => apply_update(targets, None),
        Resolved::Insert(entries) => apply_insert(entries, None),
    }
}

fn unlink(dirfd: &OwnedFd, name: &CString, kind: Kind, path: &Path) -> Result<()> {
    let flags = if kind == Kind::Dir {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    rustix::fs::unlinkat(dirfd, name, flags).map_err(|e| io(path, e))
}

fn open_read(dirfd: &OwnedFd, name: &CString, path: &Path) -> Result<std::fs::File> {
    let fd = rustix::fs::openat(
        dirfd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io(path, e))?;
    Ok(std::fs::File::from(fd))
}

fn copy_attributes(from: &std::fs::File, from_path: &Path, to: &std::fs::File) -> Result<()> {
    let stat = rustix::fs::statx(from, c"", AtFlags::EMPTY_PATH, STATX_MASK)
        .map_err(|e| io(from_path, e))?;
    rustix::fs::fchmod(to, Mode::from_raw_mode(u32::from(stat.stx_mode) & 0o7777))
        .map_err(|e| io(from_path, e))?;
    let _ = rustix::fs::fchown(
        to,
        Some(Uid::from_raw(stat.stx_uid)),
        Some(Gid::from_raw(stat.stx_gid)),
    );
    for name in xattr::names(from_path)? {
        if let Some(value) = xattr::value(from_path, &name)? {
            match rustix::fs::fsetxattr(to, OsStr::from_bytes(&name), &value, XattrFlags::empty()) {
                Ok(()) | Err(Errno::OPNOTSUPP | Errno::PERM | Errno::ACCESS) => {}
                Err(e) => return Err(io(from_path, e)),
            }
        }
    }
    let times = Timestamps {
        last_access: Timespec {
            tv_sec: stat.stx_atime.tv_sec,
            tv_nsec: i64::from(stat.stx_atime.tv_nsec),
        },
        last_modification: Timespec {
            tv_sec: stat.stx_mtime.tv_sec,
            tv_nsec: i64::from(stat.stx_mtime.tv_nsec),
        },
    };
    rustix::fs::futimens(to, &times).map_err(|e| io(from_path, e))
}

fn copy_file(
    mut source: std::fs::File,
    source_path: &Path,
    mut sink: std::fs::File,
    sink_path: &Path,
) -> Result<()> {
    std::io::copy(&mut source, &mut sink).map_err(|e| std_io(sink_path, e))?;
    copy_attributes(&source, source_path, &sink)?;
    sink.sync_all().map_err(|e| std_io(sink_path, e))
}

fn move_out(dirfd: &OwnedFd, name: &CString, path: &Path, tomb: &Path) -> Result<()> {
    let source = open_read(dirfd, name, path)?;
    let tomb_dir = open_chain(tomb.parent().expect("tomb parent"))?;
    let sink = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tomb)
        .map_err(|e| std_io(tomb, e))?;
    copy_and_remove(source, path, sink, tomb, &tomb_dir, || {
        unlink(dirfd, name, Kind::File, path)
    })
}

fn copy_and_remove(
    source: std::fs::File,
    source_path: &Path,
    sink: std::fs::File,
    sink_path: &Path,
    destination_dir: &OwnedFd,
    remove_source: impl FnOnce() -> Result<()>,
) -> Result<()> {
    copy_file(source, source_path, sink, sink_path)?;
    // Persist both the bytes and their directory entry before removing the
    // only other copy. A pending journal intent cannot recover lost bytes.
    rustix::fs::fsync(destination_dir).map_err(|e| io(sink_path, e))?;
    remove_source()
}

fn move_in(tomb: &Path, dirfd: &OwnedFd, name: &CString, path: &Path) -> Result<()> {
    let source = std::fs::File::open(tomb).map_err(|e| std_io(tomb, e))?;
    let fd = rustix::fs::openat(
        dirfd,
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| io(path, e))?;
    copy_and_remove(source, tomb, std::fs::File::from(fd), path, dirfd, || {
        std::fs::remove_file(tomb).map_err(|e| std_io(tomb, e))
    })
}

fn apply_delete(targets: &[Target], mut journal: Option<&mut Journal>) -> Result<Outcome> {
    verify_all(targets)?;
    if journal.is_some()
        && targets
            .iter()
            .any(|t| !matches!(t.frozen.kind, Kind::File | Kind::Dir | Kind::Symlink))
    {
        return Err(Error::Unsupported(
            "journaled deletion of special files".into(),
        ));
    }
    let mut order: Vec<&Target> = targets.iter().collect();
    order.sort_by_key(|t| Reverse(t.frozen.path.components().count()));
    let mut outcome = Outcome::default();
    for target in order {
        let frozen = &target.frozen;
        let step = (|| -> Result<()> {
            let (dirfd, name) = check(frozen)?;
            let Some(journal) = journal.as_deref_mut() else {
                return unlink(&dirfd, &name, frozen.kind, &frozen.path);
            };
            let seq = journal.next_seq();
            let tomb = journal.tomb_path(seq);
            let record = Record::Delete {
                seq,
                path: frozen.path.as_os_str().as_bytes().to_vec(),
                kind: frozen.kind.as_str().to_owned(),
                mode: target.before.mode,
                target: target.before.target.clone(),
                tomb: (frozen.kind == Kind::File).then(|| tomb.as_os_str().as_bytes().to_vec()),
                dev: frozen.identity.dev,
                ino: frozen.identity.ino,
                ctime: frozen.identity.ctime,
            };
            journal.begin(&record)?;
            let _tomb_bytes = match frozen.kind {
                Kind::Dir => {
                    unlink(&dirfd, &name, Kind::Dir, &frozen.path)?;
                    None
                }
                Kind::File => match rustix::fs::renameat(&dirfd, &name, CWD, &tomb) {
                    Ok(()) => Some(tomb.as_os_str().as_bytes().to_vec()),
                    Err(Errno::XDEV) => {
                        move_out(&dirfd, &name, &frozen.path, &tomb)?;
                        Some(tomb.as_os_str().as_bytes().to_vec())
                    }
                    Err(e) => return Err(io(&frozen.path, e)),
                },
                _ => {
                    unlink(&dirfd, &name, frozen.kind, &frozen.path)?;
                    None
                }
            };
            journal::sync_dir(&frozen.parent)?;
            journal::sync_dir(tomb.parent().expect("tomb parent"))?;
            journal.record(record)
        })();
        match step {
            Ok(()) => outcome.applied += 1,
            Err(error) => {
                outcome.failures.push((frozen.path.clone(), error));
                if let Some(journal) = journal.as_deref_mut() {
                    let seq = journal.next_seq().saturating_sub(1);
                    if check(frozen).is_ok() && !journal.tomb_path(seq).exists() {
                        journal.cancel(seq)?;
                    } else {
                        outcome.recovery_required.push(journal.dir().to_path_buf());
                        break;
                    }
                }
            }
        }
    }
    Ok(outcome)
}

fn timespec(nanos: i64) -> Timespec {
    Timespec {
        tv_sec: nanos.div_euclid(time::NANOS_PER_SECOND),
        tv_nsec: nanos.rem_euclid(time::NANOS_PER_SECOND),
    }
}

fn omit() -> Timespec {
    Timespec {
        tv_sec: 0,
        tv_nsec: UTIME_OMIT,
    }
}

fn set_times(
    dirfd: &OwnedFd,
    name: &CString,
    path: &Path,
    atime: Option<i64>,
    mtime: Option<i64>,
) -> Result<()> {
    let times = Timestamps {
        last_access: atime.map(timespec).unwrap_or_else(omit),
        last_modification: mtime.map(timespec).unwrap_or_else(omit),
    };
    let fd = pin(dirfd, name, path)?;
    rustix::fs::utimensat(
        &fd,
        c"",
        &times,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH,
    )
    .map_err(|e| io(path, e))
}

fn pin(dirfd: &OwnedFd, name: &CString, path: &Path) -> Result<OwnedFd> {
    if name.as_bytes().is_empty() {
        return rustix::io::dup(dirfd).map_err(|e| io(path, e));
    }
    rustix::fs::openat(
        dirfd,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io(path, e))
}

fn set_mode(dirfd: &OwnedFd, name: &CString, path: &Path, _kind: Kind, mode: u32) -> Result<()> {
    let fd = pin(dirfd, name, path)?;
    let stat = rustix::fs::statx(
        &fd,
        c"",
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        STATX_MASK,
    )
    .map_err(|e| io(path, e))?;
    if Kind::from_mode(u32::from(stat.stx_mode)) == Kind::Symlink {
        return Err(Error::Plan(format!(
            "{} is a symlink, its mode cannot change",
            path.display()
        )));
    }
    // rustix's chmodat does not implement fchmodat2. This Linux procfs magic
    // link addresses our held O_PATH descriptor, never the mutable entry name.
    let descriptor = format!("/proc/self/fd/{}", fd.as_raw_fd());
    rustix::fs::chmodat(
        CWD,
        descriptor.as_str(),
        Mode::from_raw_mode(mode & 0o7777),
        AtFlags::empty(),
    )
    .map_err(|e| io(path, e))
}

fn set_owner(
    dirfd: &OwnedFd,
    name: &CString,
    path: &Path,
    uid: Option<u32>,
    gid: Option<u32>,
) -> Result<()> {
    let fd = pin(dirfd, name, path)?;
    rustix::fs::chownat(
        &fd,
        c"",
        uid.map(Uid::from_raw),
        gid.map(Gid::from_raw),
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH,
    )
    .map_err(|e| io(path, e))
}

fn set_target(dirfd: &OwnedFd, name: &CString, path: &Path, target: &[u8]) -> Result<OwnedFd> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temporary = CString::new(format!(
        ".fsql-link-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("temporary name");
    rustix::fs::symlinkat(OsStr::from_bytes(target), dirfd, &temporary).map_err(|e| io(path, e))?;
    let result = (|| {
        let fd = pin(dirfd, &temporary, path)?;
        rustix::fs::renameat(dirfd, &temporary, dirfd, name).map_err(|e| io(path, e))?;
        Ok(fd)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(dirfd, &temporary, AtFlags::empty());
    }
    result
}

fn rename(
    dirfd: &OwnedFd,
    name: &CString,
    path: &Path,
    destination: &Path,
) -> Result<(OwnedFd, CString)> {
    let (Some(parent), Some(new_name)) = (destination.parent(), destination.file_name()) else {
        return Err(Error::Plan(format!(
            "`{}` is not a valid destination",
            destination.display()
        )));
    };
    let new_dirfd = open_chain(parent)?;
    let new_name = c_name(new_name, destination)?;
    rustix::fs::renameat_with(dirfd, name, &new_dirfd, &new_name, RenameFlags::NOREPLACE)
        .map_err(|e| io(path, e))?;
    Ok((new_dirfd, new_name))
}

fn apply_update(targets: &[Target], mut journal: Option<&mut Journal>) -> Result<Outcome> {
    verify_all(targets)?;
    let mut outcome = Outcome::default();
    for target in targets {
        let frozen = &target.frozen;
        let before = Attrs {
            mode: Some(target.before.mode),
            uid: Some(target.before.uid),
            gid: Some(target.before.gid),
            atime: Some(target.before.atime),
            mtime: Some(target.before.mtime),
            path: Some(frozen.path.as_os_str().as_bytes().to_vec()),
            target: target.before.target.clone(),
        };
        let seq = journal.as_ref().map(|j| j.next_seq()).unwrap_or(0);
        let mut intended = Attrs::default();
        for change in &target.changes {
            match change {
                Change::Mode(v) => intended.mode = Some(*v),
                Change::Uid(v) => intended.uid = Some(*v),
                Change::Gid(v) => intended.gid = Some(*v),
                Change::Atime(v) => intended.atime = Some(*v),
                Change::Mtime(v) => intended.mtime = Some(*v),
                Change::Rename(v) => intended.path = Some(v.as_os_str().as_bytes().to_vec()),
                Change::Target(v) => intended.target = Some(v.clone()),
            }
        }
        let mut after = Attrs::default();
        let mut identity = None;
        let mut begun = false;
        let mut synchronized = true;
        let step = (|| -> Result<()> {
            let (mut dirfd, mut name) = check(frozen)?;
            let mut path = frozen.path.clone();
            if let Some(journal) = journal.as_deref_mut() {
                journal.begin(&Record::Update {
                    seq,
                    path: frozen.path.as_os_str().as_bytes().to_vec(),
                    kind: frozen.kind.as_str().into(),
                    before: before.clone(),
                    after: intended,
                    identity: Some(frozen.identity),
                })?;
                begun = true;
            }
            let mut expected = frozen.identity;
            for change in &target.changes {
                let pinned = pin(&dirfd, &name, &path)?;
                let empty = c"".to_owned();
                let stat = rustix::fs::statx(
                    &pinned,
                    c"",
                    AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
                    STATX_MASK,
                )
                .map_err(|e| io(&path, e))?;
                if !Identity::of(&stat).matches(&expected, frozen.kind) {
                    return Err(Error::Stale(path.clone()));
                }
                synchronized = false;
                let mut replacement = None;
                match change {
                    Change::Mode(mode) => {
                        set_mode(&pinned, &empty, &path, frozen.kind, *mode)?;
                        after.mode = Some(*mode);
                    }
                    Change::Uid(uid) => {
                        set_owner(&pinned, &empty, &path, Some(*uid), None)?;
                        after.uid = Some(*uid);
                    }
                    Change::Gid(gid) => {
                        set_owner(&pinned, &empty, &path, None, Some(*gid))?;
                        after.gid = Some(*gid);
                    }
                    Change::Atime(t) => {
                        set_times(&pinned, &empty, &path, Some(*t), None)?;
                        after.atime = Some(*t);
                    }
                    Change::Mtime(t) => {
                        set_times(&pinned, &empty, &path, None, Some(*t))?;
                        after.mtime = Some(*t);
                    }
                    Change::Rename(destination) => {
                        let (new_dirfd, new_name) = rename(&dirfd, &name, &path, destination)?;
                        dirfd = new_dirfd;
                        name = new_name;
                        path = destination.clone();
                        after.path = Some(destination.as_os_str().as_bytes().to_vec());
                    }
                    Change::Target(target) => {
                        replacement = Some(set_target(&dirfd, &name, &path, target)?);
                        after.target = Some(target.clone());
                    }
                }
                let stat = rustix::fs::statx(
                    replacement.as_ref().unwrap_or(&pinned),
                    c"",
                    AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
                    STATX_MASK,
                )
                .map_err(|e| io(&path, e))?;
                expected = Identity::of(&stat);
                let named = rustix::fs::statx(&dirfd, &name, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK)
                    .map_err(|e| io(&path, e))?;
                if Identity::of(&named) != expected {
                    return Err(Error::Stale(path.clone()));
                }
                identity = Some(expected);
                if journal.is_some() {
                    rustix::fs::syncfs(&dirfd).map_err(|e| io(&path, e))?;
                }
                synchronized = true;
            }
            Ok(())
        })();
        let touched = after != Attrs::default();
        if touched && let Some(journal) = journal.as_deref_mut() {
            if !synchronized {
                outcome.partial.push(frozen.path.clone());
                outcome.failures.push((
                    frozen.path.clone(),
                    step.err()
                        .unwrap_or_else(|| Error::RecoveryRequired(journal.dir().to_path_buf())),
                ));
                outcome.recovery_required.push(journal.dir().to_path_buf());
                break;
            }
            let record = Record::Update {
                seq,
                path: frozen.path.as_os_str().as_bytes().to_vec(),
                kind: frozen.kind.as_str().to_owned(),
                before: before.clone(),
                after: after.clone(),
                identity,
            };
            if let Err(error) = journal.record(record) {
                outcome.failures.push((frozen.path.clone(), error));
                outcome.recovery_required.push(journal.dir().to_path_buf());
                break;
            }
        } else if begun && let Some(journal) = journal.as_deref_mut() {
            journal.cancel(seq)?;
        }
        match step {
            Ok(()) => outcome.applied += 1,
            Err(error) => {
                if touched {
                    outcome.partial.push(frozen.path.clone());
                }
                outcome.failures.push((frozen.path.clone(), error));
            }
        }
    }
    Ok(outcome)
}

fn apply_insert(entries: &[NewEntry], journal: Option<&mut Journal>) -> Result<Outcome> {
    apply_insert_observed(entries, journal, |_| {})
}

fn apply_insert_observed(
    entries: &[NewEntry],
    mut journal: Option<&mut Journal>,
    mut after_create: impl FnMut(&Path),
) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    for entry in entries {
        let seq = journal.as_ref().map(|j| j.next_seq()).unwrap_or(0);
        let mut created = false;
        let mut begun = false;
        let mut recorded_identity = None;
        let mut synchronized = false;
        let step = (|| -> Result<()> {
            validate_path(&entry.path)?;
            let (dirfd, name) = split(&entry.path)?;
            // Open and verify the source before creating anything. The descriptor
            // remains pinned throughout the copy, even if its name is replaced.
            let mut source = if entry.kind == Kind::File && entry.content.is_none() {
                match &entry.source {
                    Some(path) => {
                        let (parent, name) = split(path)?;
                        let file = open_read(&parent, &name, path)?;
                        let stat = rustix::fs::statx(&file, c"", AtFlags::EMPTY_PATH, STATX_MASK)
                            .map_err(|e| io(path, e))?;
                        if Some(Identity::of(&stat)) != entry.source_identity {
                            return Err(Error::Stale(path.clone()));
                        }
                        Some(file)
                    }
                    None => None,
                }
            } else {
                None
            };
            if let Some(journal) = journal.as_deref_mut() {
                journal.begin(&Record::Insert {
                    seq,
                    path: entry.path.as_os_str().as_bytes().to_vec(),
                    kind: entry.kind.as_str().into(),
                    dev: 0,
                    ino: 0,
                    ctime: 0,
                })?;
                begun = true;
            }
            let mut file = match entry.kind {
                Kind::Dir => {
                    rustix::fs::mkdirat(
                        &dirfd,
                        &name,
                        Mode::from_raw_mode(entry.mode.unwrap_or(0o755)),
                    )
                    .map_err(|e| io(&entry.path, e))?;
                    None
                }
                Kind::File => {
                    let fd = rustix::fs::openat(
                        &dirfd,
                        &name,
                        OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::WRONLY
                            | OFlags::CLOEXEC
                            | OFlags::NOFOLLOW,
                        Mode::from_raw_mode(entry.mode.unwrap_or(0o644)),
                    )
                    .map_err(|e| io(&entry.path, e))?;
                    Some(std::fs::File::from(fd))
                }
                Kind::Symlink => {
                    rustix::fs::symlinkat(
                        OsStr::from_bytes(entry.target.as_deref().unwrap_or(b"")),
                        &dirfd,
                        &name,
                    )
                    .map_err(|e| io(&entry.path, e))?;
                    None
                }
                _ => return Err(Error::Unsupported("creating special files".into())),
            };
            created = true;
            // Keep the created inode pinned through copying, attributes and
            // journaling. Resolving its name again could select a replacement.
            let pinned = match &file {
                Some(file) => rustix::io::dup(file).map_err(|e| io(&entry.path, e))?,
                None => pin(&dirfd, &name, &entry.path)?,
            };
            after_create(&entry.path);
            let empty = c"".to_owned();
            let work = (|| -> Result<()> {
                if let Some(file) = file.as_mut() {
                    if let Some(content) = &entry.content {
                        std::io::Write::write_all(file, content)
                            .map_err(|e| std_io(&entry.path, e))?;
                    } else if let Some(from) = source.as_mut() {
                        std::io::copy(from, file).map_err(|e| std_io(&entry.path, e))?;
                    }
                    file.sync_all().map_err(|e| std_io(&entry.path, e))?;
                }
                if let Some(mode) = entry.mode
                    && entry.kind != Kind::Symlink
                {
                    set_mode(&pinned, &empty, &entry.path, entry.kind, mode)?;
                }
                if entry.uid.is_some() || entry.gid.is_some() {
                    set_owner(&pinned, &empty, &entry.path, entry.uid, entry.gid)?;
                }
                if entry.atime.is_some() || entry.mtime.is_some() {
                    set_times(&pinned, &empty, &entry.path, entry.atime, entry.mtime)?;
                }
                Ok(())
            })();
            let stat = rustix::fs::statx(
                &pinned,
                c"",
                AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
                STATX_MASK,
            )
            .map_err(|e| io(&entry.path, e))?;
            let identity = Identity::of(&stat);
            let named = rustix::fs::statx(&dirfd, &name, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK)
                .map_err(|e| io(&entry.path, e))?;
            if !Identity::of(&named).matches(&identity, entry.kind) {
                return Err(Error::Stale(entry.path.clone()));
            }
            recorded_identity = Some(identity);
            if journal.is_some() {
                rustix::fs::syncfs(&dirfd).map_err(|e| io(&entry.path, e))?;
            }
            synchronized = true;
            work
        })();
        if let Some(journal) = journal.as_deref_mut()
            && begun
        {
            if created {
                let logged = match recorded_identity {
                    Some(identity) if synchronized => journal.record(Record::Insert {
                        seq,
                        path: entry.path.as_os_str().as_bytes().to_vec(),
                        kind: entry.kind.as_str().into(),
                        dev: identity.dev,
                        ino: identity.ino,
                        ctime: identity.ctime,
                    }),
                    _ => Err(Error::RecoveryRequired(journal.dir().to_path_buf())),
                };
                if let Err(error) = logged {
                    outcome.partial.push(entry.path.clone());
                    outcome.failures.push((entry.path.clone(), error));
                    outcome.recovery_required.push(journal.dir().to_path_buf());
                    break;
                }
            } else {
                journal.cancel(seq)?;
            }
        }
        match step {
            Ok(()) => outcome.applied += 1,
            Err(error) => {
                if created {
                    outcome.partial.push(entry.path.clone());
                }
                outcome.failures.push((entry.path.clone(), error));
            }
        }
    }
    Ok(outcome)
}

fn split(path: &Path) -> Result<(OwnedFd, CString)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Error::Plan(format!("`{}` has no parent", path.display())));
    };
    Ok((open_chain(parent)?, c_name(name, path)?))
}

fn kind_of(text: &str) -> Kind {
    match text {
        "file" => Kind::File,
        "dir" => Kind::Dir,
        "symlink" => Kind::Symlink,
        "fifo" => Kind::Fifo,
        "socket" => Kind::Socket,
        "block" => Kind::Block,
        "char" => Kind::Char,
        _ => Kind::Unknown,
    }
}

fn is_empty_dir(dirfd: &OwnedFd, name: &CString, path: &Path) -> Result<bool> {
    let fd = rustix::fs::openat(
        dirfd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io(path, e))?;
    let entries = Dir::read_from(&fd).map_err(|e| io(path, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io(path, e))?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            return Ok(false);
        }
    }
    Ok(true)
}

fn restore_delete(
    path: &Path,
    kind: Kind,
    mode: u32,
    target: Option<&[u8]>,
    tomb: Option<&Path>,
) -> Result<()> {
    let (dirfd, name) = split(path)?;
    match (tomb, kind) {
        (Some(tomb), _) => {
            match rustix::fs::renameat_with(CWD, tomb, &dirfd, &name, RenameFlags::NOREPLACE) {
                Ok(()) => Ok(()),
                Err(Errno::XDEV) => move_in(tomb, &dirfd, &name, path),
                Err(e) => Err(io(path, e)),
            }
        }
        (None, Kind::Dir) => {
            rustix::fs::mkdirat(&dirfd, &name, Mode::from_raw_mode(mode & 0o7777))
                .map_err(|e| io(path, e))?;
            set_mode(&dirfd, &name, path, Kind::Dir, mode)
        }
        (None, Kind::Symlink) => {
            rustix::fs::symlinkat(OsStr::from_bytes(target.unwrap_or(b"")), &dirfd, &name)
                .map_err(|e| io(path, e))
        }
        (None, other) => Err(Error::Plan(format!("cannot recreate a {}", other.as_str()))),
    }
}

fn restore_update(
    original: &Path,
    kind: Kind,
    before: &Attrs,
    after: &Attrs,
    identity: Option<Identity>,
) -> Result<()> {
    let current = after
        .path
        .as_deref()
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
        .unwrap_or_else(|| original.to_path_buf());
    let (mut dirfd, mut name) = split(&current)?;
    let expected = identity.ok_or_else(|| {
        Error::Plan("legacy update journal lacks identity; automatic undo refused".into())
    })?;
    let mut pinned = pin(&dirfd, &name, &current)?;
    let empty = c"".to_owned();
    let stat = rustix::fs::statx(
        &pinned,
        c"",
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        STATX_MASK,
    )
    .map_err(|e| io(&current, e))?;
    if !Identity::of(&stat).matches(&expected, kind) {
        return Err(Error::Stale(current));
    }
    if after.path.is_some() {
        let (new_dirfd, new_name) = rename(&dirfd, &name, &current, original)?;
        dirfd = new_dirfd;
        name = new_name;
    }
    if after.target.is_some()
        && let Some(target) = &before.target
    {
        pinned = set_target(&dirfd, &name, original, target)?;
    }
    if after.mode.is_some()
        && let Some(mode) = before.mode
    {
        set_mode(&pinned, &empty, original, kind, mode)?;
    }
    if after.uid.is_some() || after.gid.is_some() {
        set_owner(
            &pinned,
            &empty,
            original,
            after.uid.and(before.uid),
            after.gid.and(before.gid),
        )?;
    }
    if after.atime.is_some() || after.mtime.is_some() {
        set_times(
            &pinned,
            &empty,
            original,
            after.atime.and(before.atime),
            after.mtime.and(before.mtime),
        )?;
    }
    rustix::fs::syncfs(&dirfd).map_err(|e| io(original, e))
}

fn restore_insert(path: &Path, kind: Kind, recorded: Identity) -> Result<()> {
    let (dirfd, name) = split(path)?;
    let stat = rustix::fs::statx(&dirfd, &name, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK)
        .map_err(|e| io(path, e))?;
    if !Identity::of(&stat).matches(&recorded, kind) {
        return Err(Error::Stale(path.to_path_buf()));
    }
    if kind == Kind::Dir && !is_empty_dir(&dirfd, &name, path)? {
        return Err(Error::Plan(format!(
            "{} is no longer empty",
            path.display()
        )));
    }
    unlink(&dirfd, &name, kind, path)
}

pub fn undo(base: &Path, id: &str) -> Result<Outcome> {
    if recovery::is_replay_journal(base, id)? {
        return recovery::undo(base, id);
    }
    let records = journal::load(base, id)?;
    let mut outcome = Outcome::default();
    for record in records.iter().rev() {
        journal::begin_undo(base, id, record.seq())?;
        let (path, step) = match record {
            Record::Delete {
                path,
                kind,
                mode,
                target,
                tomb,
                ..
            } => {
                let path = PathBuf::from(OsStr::from_bytes(path));
                let tomb = tomb.as_deref().map(|t| PathBuf::from(OsStr::from_bytes(t)));
                let result = restore_delete(
                    &path,
                    kind_of(kind),
                    *mode,
                    target.as_deref(),
                    tomb.as_deref(),
                );
                (path, result)
            }
            Record::Update {
                path,
                kind,
                before,
                after,
                identity,
                ..
            } => {
                let original = PathBuf::from(OsStr::from_bytes(path));
                let result = restore_update(&original, kind_of(kind), before, after, *identity);
                (original, result)
            }
            Record::Insert {
                path,
                kind,
                dev,
                ino,
                ctime,
                ..
            } => {
                let path = PathBuf::from(OsStr::from_bytes(path));
                let recorded = Identity {
                    dev: *dev,
                    ino: *ino,
                    ctime: *ctime,
                };
                let result = restore_insert(&path, kind_of(kind), recorded);
                (path, result)
            }
        };
        match step {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    let fd = open_chain(parent)?;
                    rustix::fs::syncfs(&fd).map_err(|e| io(&path, e))?;
                }
                journal::finish_undo(base, id, record.seq())?;
                outcome.applied += 1;
            }
            Err(error) => {
                outcome.failures.push((path, error));
                outcome.recovery_required.push(base.join(id));
                break;
            }
        }
    }
    if outcome.failures.is_empty() {
        journal::remove(base, id)?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::WalkOptions;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    struct Fixture {
        dir: PathBuf,
        base: PathBuf,
        planner: Planner,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("fsql-mutate-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let dir = root.join("tree");
            std::fs::create_dir_all(dir.join("build/nested")).expect("dirs");
            std::fs::create_dir_all(dir.join("src")).expect("dirs");
            std::fs::write(dir.join("build/a.tmp"), vec![1u8; 100]).expect("file");
            std::fs::write(dir.join("build/b.tmp"), vec![2u8; 200]).expect("file");
            std::fs::write(dir.join("build/nested/c.tmp"), vec![3u8; 300]).expect("file");
            std::fs::write(dir.join("src/keep.rs"), b"fn main() {}").expect("file");
            symlink("keep.rs", dir.join("src/link")).expect("symlink");
            let base = root.join("journal");
            let dir = std::fs::canonicalize(dir).expect("canonical");
            let planner = Planner::new(&dir, WalkOptions::default());
            Self { dir, base, planner }
        }

        fn plan(&self, sql: &str) -> Plan {
            self.planner.plan(sql).expect("plan").remove(0)
        }

        fn resolve(&self, sql: &str) -> Resolved {
            let mut errors = Vec::new();
            let resolved =
                resolve(&self.plan(sql), &self.planner, &mut |e| errors.push(e)).expect("resolve");
            assert!(errors.is_empty(), "{errors:?}");
            resolved
        }

        fn run(&self, sql: &str) -> (Outcome, String) {
            let resolved = self.resolve(sql);
            let mut journal = Journal::open(&self.base, sql).expect("journal");
            let outcome = apply(&resolved, Some(&mut journal)).expect("apply");
            (outcome, journal.id().to_owned())
        }

        fn exists(&self, rel: &str) -> bool {
            std::fs::symlink_metadata(self.dir.join(rel)).is_ok()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.dir.parent().expect("root"));
        }
    }

    #[test]
    fn resolve_is_a_dry_run() {
        let fx = Fixture::new("dry");
        let resolved = fx.resolve("delete from files where ext = 'tmp'");
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved.bytes(), 600);
        assert!(fx.exists("build/a.tmp"));
    }

    #[test]
    fn insert_keeps_replacement_objects_out_of_its_journal_and_attributes() {
        let fx = Fixture::new("insert-replaced");
        let destination = fx.dir.join("created");
        let moved = fx.dir.join("moved");
        let sql = format!(
            "insert into files(path, content, mode) values ('{}', 'original content', 0o600)",
            destination.display()
        );
        let Resolved::Insert(entries) = fx.resolve(&sql) else {
            panic!("insert")
        };
        let mut journal = Journal::open(&fx.base, &sql).unwrap();
        let outcome = apply_insert_observed(&entries, Some(&mut journal), |path| {
            std::fs::rename(path, &moved).unwrap();
            std::fs::write(path, b"unrelated replacement").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
        })
        .unwrap();
        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.partial, vec![destination.clone()]);
        assert_eq!(outcome.recovery_required, vec![journal.dir().to_owned()]);
        assert!(matches!(
            undo(&fx.base, journal.id()),
            Err(Error::RecoveryRequired(_))
        ));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"unrelated replacement"
        );
        assert_eq!(
            std::fs::metadata(&destination).unwrap().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read(moved).unwrap(), b"original content");
        assert!(
            std::fs::read(journal.dir().join("log.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn failed_directory_sync_preserves_the_source_copy() {
        let fx = Fixture::new("copy-sync-failure");
        let source = fx.dir.join("src/keep.rs");
        let destination = fx.dir.join("restored");
        // O_PATH supports openat but deliberately cannot be fsynced. This
        // injects a durability failure after the copied file has been flushed.
        let dirfd = rustix::fs::openat(
            CWD,
            &fx.dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let result = move_in(
            &source,
            &dirfd,
            &c_name(OsStr::new("restored"), &destination).unwrap(),
            &destination,
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"fn main() {}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"fn main() {}");
    }

    #[test]
    fn cross_filesystem_delete_and_undo_preserve_content() {
        let fx = Fixture::new("cross-device");
        if std::fs::metadata(&fx.dir).unwrap().dev() == std::fs::metadata("/dev/shm").unwrap().dev()
        {
            return;
        }
        let base = PathBuf::from(format!("/dev/shm/fsql-cross-device-{}", std::process::id()));
        std::fs::create_dir(&base).unwrap();
        let resolved = fx.resolve("delete from files where name = 'keep.rs'");
        let mut journal = Journal::open(&base, "delete").unwrap();
        let outcome = apply(&resolved, Some(&mut journal)).unwrap();
        assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
        assert!(!fx.exists("src/keep.rs"));
        let undone = undo(&base, journal.id()).unwrap();
        assert_eq!(undone.applied, 1, "{:?}", undone.failures);
        assert_eq!(
            std::fs::read(fx.dir.join("src/keep.rs")).unwrap(),
            b"fn main() {}"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn delete_moves_files_to_tombstones_and_undo_restores_them() {
        let fx = Fixture::new("delete");
        let (outcome, id) = fx.run("delete from files f where f.ext = 'tmp'");
        assert_eq!(outcome.applied, 3, "{:?}", outcome.failures);
        assert!(!fx.exists("build/a.tmp"));
        assert!(!fx.exists("build/nested/c.tmp"));
        assert!(fx.exists("src/keep.rs"));
        let records = journal::load(&fx.base, &id).expect("load");
        assert_eq!(records.len(), 3);
        assert!(
            records
                .iter()
                .all(|r| matches!(r, Record::Delete { tomb: Some(_), .. }))
        );
        let undone = undo(&fx.base, &id).expect("undo");
        assert_eq!(undone.applied, 3, "{:?}", undone.failures);
        assert!(fx.exists("build/a.tmp"));
        assert_eq!(
            std::fs::read(fx.dir.join("build/nested/c.tmp")).expect("read"),
            vec![3u8; 300]
        );
        assert!(journal::list(&fx.base).expect("list").is_empty());
    }

    #[test]
    fn delete_removes_directories_only_when_the_predicate_matches_their_contents() {
        let fx = Fixture::new("dirs");
        let (outcome, _) = fx.run("delete from files where path glob '*/build/nested*'");
        assert_eq!(outcome.applied, 2, "{:?}", outcome.failures);
        assert!(!fx.exists("build/nested"));
        let (outcome, _) = fx.run("delete from files where name = 'build'");
        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.failures.len(), 1);
        assert!(fx.exists("build/a.tmp"));
    }

    #[test]
    fn delete_with_a_subquery_predicate() {
        let fx = Fixture::new("subquery");
        let resolved = fx.resolve(
            "delete from files where size = (select max(size) from files where ext = 'tmp')",
        );
        assert_eq!(resolved.paths()[0].0, fx.dir.join("build/nested/c.tmp"));
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn delete_aborts_entirely_when_a_target_changed() {
        let fx = Fixture::new("stale");
        let resolved = fx.resolve("delete from files where ext = 'tmp'");
        std::fs::write(fx.dir.join("build/a.tmp"), b"changed").expect("rewrite");
        let mut journal = Journal::open(&fx.base, "x").expect("journal");
        assert!(matches!(
            apply(&resolved, Some(&mut journal)),
            Err(Error::Stale(_))
        ));
        assert!(fx.exists("build/b.tmp"));
        assert!(fx.exists("build/nested/c.tmp"));
    }

    #[test]
    fn delete_ordered_with_limit_targets_the_largest_first() {
        let fx = Fixture::new("limit");
        let resolved = fx.resolve("delete from files where ext = 'tmp' order by size desc limit 1");
        assert_eq!(resolved.paths()[0].0, fx.dir.join("build/nested/c.tmp"));
    }

    #[test]
    fn update_changes_mode_and_renames_with_undo() {
        let fx = Fixture::new("update");
        let (outcome, id) =
            fx.run("update files set mode = 0o600, name = 'renamed.rs' where name = 'keep.rs'");
        assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
        assert!(!fx.exists("src/keep.rs"));
        let meta = std::fs::metadata(fx.dir.join("src/renamed.rs")).expect("meta");
        assert_eq!(meta.mode() & 0o777, 0o600);
        let undone = undo(&fx.base, &id).expect("undo");
        assert_eq!(undone.applied, 1, "{:?}", undone.failures);
        let meta = std::fs::metadata(fx.dir.join("src/keep.rs")).expect("meta");
        assert_eq!(meta.mode() & 0o777, 0o644);
    }

    #[test]
    fn update_sets_timestamps_and_symlink_targets() {
        let fx = Fixture::new("times");
        let (outcome, _) =
            fx.run("update files set mtime = '2020-01-02T03:04:05Z' where name = 'keep.rs'");
        assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
        let meta = std::fs::metadata(fx.dir.join("src/keep.rs")).expect("meta");
        assert_eq!(meta.mtime(), 1_577_934_245);
        let (outcome, id) = fx.run("update files set target = 'elsewhere' where name = 'link'");
        assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
        assert_eq!(
            std::fs::read_link(fx.dir.join("src/link")).expect("link"),
            PathBuf::from("elsewhere")
        );
        undo(&fx.base, &id).expect("undo");
        assert_eq!(
            std::fs::read_link(fx.dir.join("src/link")).expect("link"),
            PathBuf::from("keep.rs")
        );
        let resolved = fx.resolve("update files set mode = 0o600 where name = 'link'");
        let mut journal = Journal::open(&fx.base, "x").expect("journal");
        let outcome = apply(&resolved, Some(&mut journal)).expect("apply");
        assert_eq!(outcome.failures.len(), 1);
    }

    #[test]
    fn insert_creates_dirs_files_and_symlinks_with_undo() {
        let fx = Fixture::new("insert");
        let sql = format!(
            "insert into files (path, kind, mode, target) values ('{0}/newdir', 'dir', 0o700, null), ('{0}/newdir/file', 'file', null, null), ('{0}/newlink', 'symlink', null, 'newdir')",
            fx.dir.display()
        );
        let (outcome, id) = fx.run(&sql);
        assert_eq!(outcome.applied, 3, "{:?}", outcome.failures);
        assert_eq!(
            std::fs::metadata(fx.dir.join("newdir"))
                .expect("dir")
                .mode()
                & 0o777,
            0o700
        );
        assert!(fx.exists("newdir/file"));
        assert_eq!(
            std::fs::read_link(fx.dir.join("newlink")).expect("link"),
            PathBuf::from("newdir")
        );
        let (outcome, _) = fx.run(&sql);
        assert_eq!(outcome.applied, 0);
        assert_eq!(outcome.failures.len(), 3);
        let undone = undo(&fx.base, &id).expect("undo");
        assert_eq!(undone.applied, 3, "{:?}", undone.failures);
        assert!(!fx.exists("newdir"));
        assert!(!fx.exists("newlink"));
    }

    #[test]
    fn insert_select_copies_content_from_source() {
        let fx = Fixture::new("insert-select");
        let (outcome, _) = fx.run("insert into files (path, source) select path || '.bak', path from files where ext = 'rs'");
        assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
        assert_eq!(
            std::fs::read(fx.dir.join("src/keep.rs.bak")).expect("read"),
            b"fn main() {}"
        );
        let (outcome, _) = fx.run(&format!(
            "insert into files (path, content, mode) values ('{}/src/gen.txt', 'hello', 0o600)",
            fx.dir.display()
        ));
        assert_eq!(outcome.applied, 1, "{:?}", outcome.failures);
        assert_eq!(
            std::fs::read(fx.dir.join("src/gen.txt")).expect("read"),
            b"hello"
        );
        assert_eq!(
            std::fs::metadata(fx.dir.join("src/gen.txt"))
                .expect("meta")
                .mode()
                & 0o777,
            0o600
        );
        let (outcome, _) = fx.run(&format!(
            "insert into files (path, source) values ('{0}/build2', '{0}/build'), ('{0}/link2', '{0}/src/link')",
            fx.dir.display()
        ));
        assert_eq!(outcome.applied, 2, "{:?}", outcome.failures);
        assert!(
            std::fs::metadata(fx.dir.join("build2"))
                .expect("meta")
                .is_dir()
        );
        assert_eq!(
            std::fs::read_link(fx.dir.join("link2")).expect("link"),
            PathBuf::from("keep.rs")
        );
    }

    #[test]
    fn deleting_without_a_journal_unlinks_directly() {
        let fx = Fixture::new("nojournal");
        let resolved = fx.resolve("delete from files where name = 'a.tmp'");
        let outcome = apply(&resolved, None).expect("apply");
        assert_eq!(outcome.applied, 1);
        assert!(!fx.exists("build/a.tmp"));
    }

    #[test]
    fn copied_tombstones_keep_attributes() {
        let fx = Fixture::new("copy");
        let file = fx.dir.join("src/keep.rs");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        let (dirfd, name) = split(&file).expect("split");
        let tomb = fx.base.join("tomb-copy");
        std::fs::create_dir_all(&fx.base).expect("base");
        let original = std::fs::metadata(&file).expect("meta");
        move_out(&dirfd, &name, &file, &tomb).expect("move out");
        let copied = std::fs::metadata(&tomb).expect("meta");
        assert_eq!(copied.mode() & 0o777, 0o640);
        assert_eq!(copied.mtime(), original.mtime());
        assert!(!file.exists());
        move_in(&tomb, &dirfd, &name, &file).expect("move in");
        assert!(!tomb.exists());
        let restored = std::fs::metadata(&file).expect("meta");
        assert_eq!(restored.mode() & 0o777, 0o640);
        assert_eq!(std::fs::read(&file).expect("read"), b"fn main() {}");
    }
}
