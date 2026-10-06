use std::ffi::{CStr, CString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rustix::fs::{CWD, Mode, OFlags};
use rustix::io::Errno;
use rustix::process::{Resource, getrlimit};
use tree_fucker::domain::DomainCrossing;
use tree_fucker::entry::EntryKind;
use tree_fucker::fs::{FsError, ObservedKind};
use tree_fucker::path::RelativePath;
use tree_fucker::policy::{PathPredicate, ScanDecision, ScanPolicy};
use tree_fucker::scan::{Scan, ScanEntry, ScanEvent, ScanFailure, ScanOptions};
use tree_fucker::std_fs::StdFileSystem;
use tree_fucker::{HostConfig, HostGovernor, HostGovernorError};

use crate::error::{Error, Result};
use crate::row::{Entry, Kind, Shared};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    pub max_depth: Option<u32>,
    pub one_filesystem: bool,
    pub initial_depth: u32,
}

/// Private recovery directories reserve `.fsql-<hex>-<hex>-<decimal>` names.
pub(crate) fn recovery_name(name: &std::ffi::OsStr) -> bool {
    let Some(suffix) = name.to_str().and_then(|s| s.strip_prefix(".fsql-")) else {
        return false;
    };
    let parts: Vec<_> = suffix.split('-').collect();
    parts.len() == 3
        && parts.iter().all(|p| !p.is_empty())
        && parts[..2]
            .iter()
            .all(|p| p.bytes().all(|c| c.is_ascii_hexdigit()))
        && parts[2].bytes().all(|c| c.is_ascii_digit())
}

pub(crate) fn recovery_path(path: &Path) -> bool {
    path.components().any(|p| recovery_name(p.as_os_str()))
}

pub fn install_governor() -> std::result::Result<HostGovernor, HostGovernorError> {
    HostGovernor::install(HostConfig {
        foreground_duty: 1.0,
        domain_foreground_duty: 1.0,
        ..HostConfig::default()
    })
}

pub fn c_name(name: &std::ffi::OsStr, path: &Path) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other("path contains a NUL byte"),
    })
}

fn open_dir(dirfd: impl rustix::fd::AsFd, name: &CStr, path: &Path) -> Result<OwnedFd> {
    rustix::fs::openat(dirfd, name, DIR_FLAGS, Mode::empty()).map_err(|errno| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from(errno),
    })
}

pub fn open_chain(directory: &Path) -> Result<OwnedFd> {
    validate_path(directory)?;
    let mut fd = open_dir(CWD, c"/", Path::new("/"))?;
    let mut opened = PathBuf::from("/");
    for component in directory.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        opened.push(part);
        let c_part = c_name(part, &opened)?;
        fd = open_dir(&fd, &c_part, &opened)?;
    }
    Ok(fd)
}

/// Mutation paths are absolute and contain no parent traversal components.
pub fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(Error::Plan(format!(
            "{} must be absolute and must not contain `..`",
            path.display()
        )));
    }
    c_name(path.as_os_str(), path)?;
    Ok(())
}

fn kind_of(kind: ObservedKind) -> Kind {
    match kind {
        ObservedKind::Resolved(EntryKind::File) => Kind::File,
        ObservedKind::Resolved(EntryKind::Directory) => Kind::Dir,
        ObservedKind::Resolved(EntryKind::Symlink) => Kind::Symlink,
        ObservedKind::Resolved(EntryKind::Other) | ObservedKind::Unresolved => Kind::Unknown,
    }
}

fn io_error(error: &FsError) -> std::io::Error {
    match error {
        FsError::NotFound => Errno::NOENT.into(),
        FsError::NotDirectory => Errno::NOTDIR.into(),
        FsError::PermissionDenied => Errno::ACCESS.into(),
        other => std::io::Error::other(other.to_string()),
    }
}

fn scan_error(path: &Path, error: &tree_fucker::Error) -> Error {
    let source = match error {
        tree_fucker::Error::NotFound => Errno::NOENT.into(),
        tree_fucker::Error::NotDirectory => Errno::NOTDIR.into(),
        tree_fucker::Error::Io(error) => io_error(error),
        other => std::io::Error::other(other.to_string()),
    };
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn anchor_limit() -> usize {
    let open_files = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
    usize::try_from(open_files / 4).unwrap_or(usize::MAX)
}

pub struct Walker {
    shared: Rc<Shared>,
    scan: Scan,
    root: PathBuf,
    initial_depth: u32,
    directory: Option<(RelativePath, PathBuf)>,
    cancellation: Option<crate::execution::ScanCancellation>,
}

impl Walker {
    pub fn new(root: &Path, options: WalkOptions) -> Result<Self> {
        if recovery_path(root) {
            return Err(Error::Plan(
                "private recovery directories cannot be queried".into(),
            ));
        }
        let depth = options
            .max_depth
            .map(|max| max.saturating_sub(options.initial_depth) as usize);
        let policy: Arc<dyn ScanPolicy> =
            Arc::new(PathPredicate::new(move |path: &RelativePath, _| {
                if path.file_name().is_some_and(recovery_name) {
                    ScanDecision::Excluded
                } else {
                    ScanDecision::Eligible {
                        initially_loaded: depth.is_none_or(|max| path.depth() < max),
                    }
                }
            }));
        let scan_options = ScanOptions {
            crossing: match options.one_filesystem {
                true => DomainCrossing::Exclude,
                false => DomainCrossing::Follow,
            },
            anchors: anchor_limit(),
            ceiling: Duration::MAX,
            ..ScanOptions::default()
        };
        let scan = Scan::open(
            Arc::new(StdFileSystem::new()),
            root.to_path_buf(),
            policy,
            scan_options,
        )
        .map_err(|error| scan_error(root, &error))?;
        let root = scan.root().to_path_buf();
        if recovery_path(&root) {
            return Err(Error::Plan(
                "private recovery directories cannot be queried".into(),
            ));
        }
        Ok(Self {
            shared: Shared::new(),
            scan,
            root,
            initial_depth: options.initial_depth,
            directory: None,
            cancellation: None,
        })
    }

    pub(crate) fn cancellable(
        root: &Path,
        options: WalkOptions,
        cancellation: &crate::CancellationToken,
    ) -> Result<Self> {
        if cancellation.is_cancelled() {
            return Err(Error::ResourceLimit("cancelled".into()));
        }
        let mut walker = Self::new(root, options)?;
        walker.cancellation = Some(cancellation.register(walker.scan.cancellation()));
        Ok(walker)
    }

    fn absolute(&mut self, entry: &ScanEntry) -> PathBuf {
        let (Some(directory), Some(name)) = (entry.path.directory(), entry.path.name()) else {
            return self.root.clone();
        };
        let held = match self.directory.take() {
            Some((cached, absolute)) if cached == *directory => (cached, absolute),
            _ => (directory.clone(), directory.under(&self.root)),
        };
        let path = held.1.join(name);
        self.directory = Some(held);
        path
    }

    fn entry(&mut self, entry: ScanEntry) -> Result<Entry> {
        let depth = u32::try_from(entry.path.depth()).unwrap_or(u32::MAX);
        let path = self.absolute(&entry);
        Entry::new(
            self.shared.clone(),
            entry.anchor,
            path,
            self.initial_depth.saturating_add(depth),
            kind_of(entry.kind),
        )
    }
}

impl Iterator for Walker {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            return Some(match self.scan.next()? {
                Ok(ScanEvent::Entry(entry)) => self.entry(entry),
                Ok(ScanEvent::Boundary { .. }) => continue,
                Ok(ScanEvent::Unlisted { path, failure }) => {
                    Err(unlisted_error(&path.under(&self.root), failure))
                }
                Err(error) => Err(Error::Walk {
                    root: self.root.clone(),
                    reason: error.to_string(),
                }),
            });
        }
    }
}

fn unlisted_error(path: &Path, failure: ScanFailure) -> Error {
    match failure {
        ScanFailure::Fs(error) if !matches!(error, FsError::Fatal(_)) => Error::Io {
            path: path.to_path_buf(),
            source: io_error(&error),
        },
        other => Error::Walk {
            root: path.to_path_buf(),
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::eval::Row;
    use crate::value::Value;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fsql-walk-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deeper")).expect("dirs");
        std::fs::write(dir.join("a.txt"), b"hello").expect("file");
        std::fs::write(dir.join("sub/b.TMP"), b"x".repeat(2048)).expect("file");
        std::fs::write(dir.join("sub/deeper/.hidden"), b"").expect("file");
        symlink("a.txt", dir.join("link")).expect("symlink");
        symlink("nowhere", dir.join("dangling")).expect("symlink");
        dir
    }

    fn names(walker: Walker) -> Vec<String> {
        let mut out: Vec<String> = walker
            .map(|entry| entry.expect("entry"))
            .map(|entry| entry.path().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn walks_every_entry_including_the_root() {
        let dir = fixture("all");
        let walker = Walker::new(&dir, WalkOptions::default()).expect("walker");
        let found = names(walker);
        let canonical = std::fs::canonicalize(&dir).expect("canonical");
        let expected: Vec<String> = [
            "",
            "/a.txt",
            "/dangling",
            "/link",
            "/sub",
            "/sub/b.TMP",
            "/sub/deeper",
            "/sub/deeper/.hidden",
        ]
        .iter()
        .map(|suffix| format!("{}{suffix}", canonical.display()))
        .collect();
        assert_eq!(found, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_depth_limits_descent() {
        let dir = fixture("depth");
        let walker = Walker::new(
            &dir,
            WalkOptions {
                max_depth: Some(1),
                ..WalkOptions::default()
            },
        )
        .expect("walker");
        let found = names(walker);
        assert!(found.iter().any(|p| p.ends_with("/sub")));
        assert!(!found.iter().any(|p| p.ends_with("/sub/b.TMP")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn columns_come_from_the_dirent_and_statx() {
        let dir = fixture("columns");
        let canonical = std::fs::canonicalize(&dir).expect("canonical");
        let walker = Walker::new(&dir, WalkOptions::default()).expect("walker");
        for entry in walker {
            let entry = entry.expect("entry");
            let name = entry.name().to_string_lossy().into_owned();
            match name.as_str() {
                "a.txt" => {
                    assert_eq!(entry.column("size").expect("size"), Value::Int(5));
                    assert_eq!(
                        entry.column("kind").expect("kind"),
                        Value::Text("file".into())
                    );
                    assert_eq!(entry.column("ext").expect("ext"), Value::Text("txt".into()));
                    assert_eq!(entry.column("depth").expect("depth"), Value::Int(1));
                    assert_eq!(entry.column("hidden").expect("hidden"), Value::Bool(false));
                    assert_eq!(entry.column("target").expect("target"), Value::Null);
                    assert_eq!(entry.column("broken").expect("broken"), Value::Null);
                    assert!(matches!(
                        entry.column("mtime").expect("mtime"),
                        Value::Timestamp(_)
                    ));
                    assert!(matches!(
                        entry.column("user").expect("user"),
                        Value::Text(_)
                    ));
                }
                "b.TMP" => {
                    assert_eq!(entry.column("ext").expect("ext"), Value::Text("tmp".into()));
                    assert_eq!(entry.column("depth").expect("depth"), Value::Int(2));
                    assert_eq!(
                        entry.column("parent").expect("parent"),
                        Value::Text(canonical.join("sub").to_string_lossy().into())
                    );
                }
                ".hidden" => {
                    assert_eq!(entry.column("hidden").expect("hidden"), Value::Bool(true));
                    assert_eq!(entry.column("ext").expect("ext"), Value::Null);
                    assert_eq!(entry.column("depth").expect("depth"), Value::Int(3));
                }
                "link" => {
                    assert_eq!(
                        entry.column("kind").expect("kind"),
                        Value::Text("symlink".into())
                    );
                    assert_eq!(
                        entry.column("target").expect("target"),
                        Value::Text("a.txt".into())
                    );
                    assert_eq!(entry.column("broken").expect("broken"), Value::Bool(false));
                }
                "dangling" => {
                    assert_eq!(entry.column("broken").expect("broken"), Value::Bool(true));
                    assert_eq!(
                        entry.column("target").expect("target"),
                        Value::Text("nowhere".into())
                    );
                }
                "sub" | "deeper" => {
                    assert_eq!(
                        entry.column("kind").expect("kind"),
                        Value::Text("dir".into())
                    );
                }
                _ => {
                    assert_eq!(entry.depth(), 0);
                    assert_eq!(
                        entry.column("kind").expect("kind"),
                        Value::Text("dir".into())
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frozen_rows_carry_identity() {
        let dir = fixture("frozen");
        let canonical = std::fs::canonicalize(&dir).expect("canonical");
        let walker = Walker::new(&dir, WalkOptions::default()).expect("walker");
        let frozen: Vec<_> = walker
            .map(|e| e.expect("entry").freeze().expect("freeze"))
            .collect();
        let file = frozen.iter().find(|f| f.name == "a.txt").expect("a.txt");
        assert_eq!(file.parent, canonical);
        assert_eq!(file.kind, Kind::File);
        assert!(file.identity.ino > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_read_their_columns_through_the_directory_they_were_listed_from() {
        let dir = fixture("anchored");
        let walker = Walker::new(&dir, WalkOptions::default()).expect("walker");
        let rows: Vec<Entry> = walker.map(|entry| entry.expect("entry")).collect();
        assert!(
            rows.iter()
                .filter(|row| row.depth() > 0)
                .all(Entry::anchored)
        );
        let moved = dir.with_file_name(format!("fsql-walk-{}-anchored-moved", std::process::id()));
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::rename(&dir, &moved).expect("rename");
        let file = rows
            .iter()
            .find(|row| row.name() == "a.txt")
            .expect("a.txt");
        assert_eq!(file.column("size").expect("size"), Value::Int(5));
        let link = rows.iter().find(|row| row.name() == "link").expect("link");
        assert_eq!(
            link.column("target").expect("target"),
            Value::Text("a.txt".into())
        );
        let _ = std::fs::remove_dir_all(&moved);
    }

    #[test]
    fn a_depth_limit_counts_from_the_initial_depth() {
        let dir = fixture("offset");
        let walker = Walker::new(
            &dir,
            WalkOptions {
                max_depth: Some(3),
                initial_depth: 2,
                ..WalkOptions::default()
            },
        )
        .expect("walker");
        let rows: Vec<Entry> = walker.map(|entry| entry.expect("entry")).collect();
        assert!(
            rows.iter()
                .any(|row| row.name() == "sub" && row.depth() == 3)
        );
        assert!(rows.iter().all(|row| row.depth() <= 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_root_is_an_io_error() {
        let result = Walker::new(Path::new("/definitely/not/here"), WalkOptions::default());
        assert!(matches!(result, Err(Error::Io { .. })));
    }

    #[test]
    fn an_unreadable_subdirectory_is_reported_and_skipped() {
        let dir = fixture("unreadable");
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).expect("dir");
        std::fs::write(locked.join("secret"), b"x").expect("file");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let walker = Walker::new(&dir, WalkOptions::default()).expect("walker");
        let (ok, errors): (Vec<_>, Vec<_>) = walker.partition(Result::is_ok);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let messages: Vec<String> = errors
            .iter()
            .filter_map(|e| e.as_ref().err())
            .map(ToString::to_string)
            .collect();
        if uzers::get_effective_uid() != 0 {
            assert_eq!(messages.len(), 1, "{messages:?}");
        }
        assert!(
            ok.iter()
                .any(|e| e.as_ref().is_ok_and(|e| e.name() == "locked"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn cancellation_reaches_the_underlying_scan() {
        let dir = fixture("cancel");
        let cancellation = crate::CancellationToken::default();
        let mut walker =
            Walker::cancellable(&dir, WalkOptions::default(), &cancellation).expect("walker");
        walker.next().expect("root").expect("entry");
        cancellation.cancel();
        assert!(matches!(walker.next(), Some(Err(Error::Walk { .. }))));
        assert!(walker.next().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resource_limits_never_become_skippable_io_errors() {
        use tree_fucker::update::{ResourceLimit, ResourceLimited};
        let failure = ScanFailure::ResourceLimited(ResourceLimited {
            limit: ResourceLimit::EntriesPerDirectory,
            configured: 1,
            observed: 2,
            domain: None,
        });
        assert!(matches!(
            unlisted_error(Path::new("/fixture"), failure),
            Error::Walk { .. }
        ));
        assert!(matches!(
            unlisted_error(
                Path::new("/fixture"),
                ScanFailure::Fs(FsError::PermissionDenied)
            ),
            Error::Io { .. }
        ));
    }
}
