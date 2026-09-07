use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use rustix::fs::{AtFlags, CWD, Dir, Mode, OFlags, StatxFlags};

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

struct Frame {
    fd: Arc<OwnedFd>,
    path: PathBuf,
    depth: u32,
    entries: Dir,
}

pub struct Walker {
    shared: Rc<Shared>,
    options: WalkOptions,
    root_mount: Option<u64>,
    pending: VecDeque<Result<Entry>>,
    stack: Vec<Frame>,
}

pub fn c_name(name: &std::ffi::OsStr, path: &Path) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other("path contains a NUL byte"),
    })
}

pub fn open_chain(directory: &Path) -> Result<OwnedFd> {
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

pub fn open_root(root: &Path) -> Result<(Arc<OwnedFd>, CString, PathBuf)> {
    let canonical = std::fs::canonicalize(root).map_err(|source| Error::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let (parent, name) = match (canonical.parent(), canonical.file_name()) {
        (Some(parent), Some(name)) => (parent.to_path_buf(), c_name(name, &canonical)?),
        _ => (canonical.clone(), c".".to_owned()),
    };
    let fd = open_chain(&parent)?;
    Ok((Arc::new(fd), name, canonical))
}

fn open_dir(dirfd: impl rustix::fd::AsFd, name: &CStr, path: &Path) -> Result<OwnedFd> {
    rustix::fs::openat(dirfd, name, DIR_FLAGS, Mode::empty()).map_err(|errno| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from(errno),
    })
}

impl Walker {
    pub fn new(root: &Path, options: WalkOptions) -> Result<Self> {
        let shared = Shared::new();
        let (parent, name, canonical) = open_root(root)?;
        let root_entry = Entry::new(
            shared.clone(),
            parent,
            name,
            canonical,
            options.initial_depth,
            Kind::Dir,
        );
        let root_mount = if options.one_filesystem {
            root_entry.mount_id()?
        } else {
            None
        };
        let mut walker = Self {
            shared,
            options,
            root_mount,
            pending: VecDeque::new(),
            stack: Vec::new(),
        };
        walker.descend(&root_entry);
        walker.pending.push_back(Ok(root_entry));
        Ok(walker)
    }

    fn descend(&mut self, entry: &Entry) {
        if let Some(max) = self.options.max_depth
            && entry.depth() >= max
        {
            return;
        }
        if self.root_mount.is_some() {
            match entry.mount_id() {
                Ok(mount) if mount != self.root_mount => return,
                Ok(_) => {}
                Err(error) => {
                    self.pending.push_back(Err(error));
                    return;
                }
            }
        }
        let fd = match open_dir(entry.dir(), entry.name_c(), entry.path()) {
            Ok(fd) => Arc::new(fd),
            Err(error) => {
                self.pending.push_back(Err(error));
                return;
            }
        };
        if self.root_mount.is_some() {
            let stat = rustix::fs::statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID);
            match stat {
                Ok(stat)
                    if stat.stx_mask & StatxFlags::MNT_ID.bits() != 0
                        && Some(stat.stx_mnt_id) != self.root_mount =>
                {
                    return;
                }
                _ => {}
            }
        }
        let entries = match Dir::read_from(&fd) {
            Ok(entries) => entries,
            Err(errno) => {
                self.pending.push_back(Err(Error::Io {
                    path: entry.path().to_path_buf(),
                    source: std::io::Error::from(errno),
                }));
                return;
            }
        };
        self.stack.push(Frame {
            fd,
            path: entry.path().to_path_buf(),
            depth: entry.depth() + 1,
            entries,
        });
    }

    fn next_from_stack(&mut self) -> Option<Result<Entry>> {
        loop {
            let frame = self.stack.last_mut()?;
            let Some(dirent) = frame.entries.next() else {
                self.stack.pop();
                continue;
            };
            let dirent = match dirent {
                Ok(dirent) => dirent,
                Err(errno) => {
                    let path = frame.path.clone();
                    self.stack.pop();
                    return Some(Err(Error::Io {
                        path,
                        source: std::io::Error::from(errno),
                    }));
                }
            };
            let name = dirent.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let path = frame
                .path
                .join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            let entry = Entry::new(
                self.shared.clone(),
                frame.fd.clone(),
                name.to_owned(),
                path,
                frame.depth,
                Kind::from_file_type(dirent.file_type()),
            );
            match entry.is_dir() {
                Ok(true) => self.descend(&entry),
                Ok(false) => {}
                Err(error) => {
                    self.pending.push_back(Err(error));
                    continue;
                }
            }
            return Some(Ok(entry));
        }
    }
}

impl Iterator for Walker {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(pending) = self.pending.pop_front() {
            return Some(pending);
        }
        self.next_from_stack()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Row;
    use crate::value::Value;
    use std::os::unix::fs::{PermissionsExt, symlink};

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
        .map(|suffix| format!("{}{suffix}", dir.display()))
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
                        Value::Text(dir.join("sub").to_string_lossy().into())
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
        let walker = Walker::new(&dir, WalkOptions::default()).expect("walker");
        let frozen: Vec<_> = walker
            .map(|e| e.expect("entry").freeze().expect("freeze"))
            .collect();
        let file = frozen.iter().find(|f| f.name == "a.txt").expect("a.txt");
        assert_eq!(file.parent, dir);
        assert_eq!(file.kind, Kind::File);
        assert!(file.identity.ino > 0);
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
}
