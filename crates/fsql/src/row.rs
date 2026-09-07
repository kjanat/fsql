use std::cell::OnceCell;
use std::ffi::{CString, OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use rustix::fs::{AtFlags, FileType, Statx, StatxFlags};
use uzers::{Groups, Users, UsersCache};

use crate::column::Column;
use crate::error::{Error, Result};
use crate::eval::{Row, from_bytes};
use crate::time;
use crate::value::Value;

pub const STATX_MASK: StatxFlags = StatxFlags::BASIC_STATS
    .union(StatxFlags::BTIME)
    .union(StatxFlags::MNT_ID);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    Block,
    Char,
    Unknown,
}

impl Kind {
    pub fn from_file_type(file_type: FileType) -> Self {
        match file_type {
            FileType::RegularFile => Self::File,
            FileType::Directory => Self::Dir,
            FileType::Symlink => Self::Symlink,
            FileType::Fifo => Self::Fifo,
            FileType::Socket => Self::Socket,
            FileType::BlockDevice => Self::Block,
            FileType::CharacterDevice => Self::Char,
            FileType::Unknown => Self::Unknown,
        }
    }

    pub fn from_mode(mode: u32) -> Self {
        Self::from_file_type(FileType::from_raw_mode(mode))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Symlink => "symlink",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
            Self::Block => "block",
            Self::Char => "char",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub ctime: i64,
}

impl Identity {
    pub fn matches(&self, other: &Self, kind: Kind) -> bool {
        if kind == Kind::Dir {
            self.dev == other.dev && self.ino == other.ino
        } else {
            self == other
        }
    }

    pub fn of(stat: &Statx) -> Self {
        Self {
            dev: makedev(stat.stx_dev_major, stat.stx_dev_minor),
            ino: stat.stx_ino,
            ctime: time::from_parts(stat.stx_ctime.tv_sec, stat.stx_ctime.tv_nsec).0,
        }
    }
}

pub fn makedev(major: u32, minor: u32) -> u64 {
    rustix::fs::makedev(major, minor)
}

#[derive(Default)]
pub struct Shared {
    users: UsersCache,
}

impl Shared {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    fn user_name(&self, uid: u32) -> Option<OsString> {
        self.users
            .get_user_by_uid(uid)
            .map(|user| user.name().to_os_string())
    }

    fn group_name(&self, gid: u32) -> Option<OsString> {
        self.users
            .get_group_by_gid(gid)
            .map(|group| group.name().to_os_string())
    }
}

#[derive(Debug, Clone)]
pub struct Frozen {
    pub parent: PathBuf,
    pub name: OsString,
    pub path: PathBuf,
    pub kind: Kind,
    pub identity: Identity,
}

pub struct Entry {
    shared: Rc<Shared>,
    dir: Arc<OwnedFd>,
    name: CString,
    path: PathBuf,
    depth: u32,
    kind_hint: Kind,
    stat: OnceCell<std::result::Result<Statx, rustix::io::Errno>>,
    target: OnceCell<Option<Vec<u8>>>,
}

impl Entry {
    pub fn new(
        shared: Rc<Shared>,
        dir: Arc<OwnedFd>,
        name: CString,
        path: PathBuf,
        depth: u32,
        kind_hint: Kind,
    ) -> Self {
        Self {
            shared,
            dir,
            name,
            path,
            depth,
            kind_hint,
            stat: OnceCell::new(),
            target: OnceCell::new(),
        }
    }

    pub fn dir(&self) -> &Arc<OwnedFd> {
        &self.dir
    }

    pub fn name(&self) -> &OsStr {
        OsStr::from_bytes(self.name.as_bytes())
    }

    pub fn name_c(&self) -> &CString {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn stat(&self) -> Result<&Statx> {
        self.stat
            .get_or_init(|| {
                rustix::fs::statx(&self.dir, &self.name, AtFlags::SYMLINK_NOFOLLOW, STATX_MASK)
            })
            .as_ref()
            .map_err(|errno| Error::Io {
                path: self.path.clone(),
                source: std::io::Error::from(*errno),
            })
    }

    pub fn kind(&self) -> Result<Kind> {
        if self.kind_hint != Kind::Unknown {
            return Ok(self.kind_hint);
        }
        Ok(Kind::from_mode(u32::from(self.stat()?.stx_mode)))
    }

    pub fn is_dir(&self) -> Result<bool> {
        Ok(self.kind()? == Kind::Dir)
    }

    pub fn mount_id(&self) -> Result<Option<u64>> {
        let stat = self.stat()?;
        Ok((stat.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id))
    }

    pub fn identity(&self) -> Result<Identity> {
        Ok(Identity::of(self.stat()?))
    }

    pub fn freeze(&self) -> Result<Frozen> {
        Ok(Frozen {
            parent: self
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            name: self.name().to_os_string(),
            path: self.path.clone(),
            kind: self.kind()?,
            identity: self.identity()?,
        })
    }

    fn target(&self) -> Result<Option<&[u8]>> {
        if self.kind()? != Kind::Symlink {
            return Ok(None);
        }
        let target = self.target.get_or_init(|| {
            rustix::fs::readlinkat(&self.dir, &self.name, Vec::new())
                .ok()
                .map(CString::into_bytes)
        });
        Ok(target.as_deref())
    }

    fn broken(&self) -> Result<Option<bool>> {
        if self.kind()? != Kind::Symlink {
            return Ok(None);
        }
        let followed = rustix::fs::statx(&self.dir, &self.name, AtFlags::empty(), StatxFlags::TYPE);
        Ok(Some(followed.is_err()))
    }
}

fn perms(mode: u32) -> String {
    let mut out = String::with_capacity(9);
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (index, (bit, ch)) in bits.iter().enumerate() {
        let set = mode & bit != 0;
        let special = match index {
            2 => mode & 0o4000 != 0,
            5 => mode & 0o2000 != 0,
            8 => mode & 0o1000 != 0,
            _ => false,
        };
        out.push(match (set, special, index) {
            (true, true, 8) => 't',
            (false, true, 8) => 'T',
            (true, true, _) => 's',
            (false, true, _) => 'S',
            (true, false, _) => *ch,
            (false, false, _) => '-',
        });
    }
    out
}

fn timestamp(seconds: i64, nanos: u32) -> Value {
    Value::Timestamp(time::from_parts(seconds, nanos))
}

impl Row for Entry {
    fn column(&self, name: &str) -> Result<Value> {
        let column = Column::parse(name).ok_or_else(|| Error::UnknownColumn(name.to_owned()))?;
        Ok(match column {
            Column::Path => from_bytes(self.path.as_os_str().as_bytes()),
            Column::Name => from_bytes(self.name.as_bytes()),
            Column::Parent => self
                .path
                .parent()
                .map(|p| from_bytes(p.as_os_str().as_bytes()))
                .unwrap_or(Value::Null),
            Column::Ext => Path::new(self.name())
                .extension()
                .map(|ext| {
                    let bytes = ext.as_bytes();
                    match std::str::from_utf8(bytes) {
                        Ok(s) => Value::Text(s.to_lowercase()),
                        Err(_) => Value::Blob(bytes.to_ascii_lowercase()),
                    }
                })
                .unwrap_or(Value::Null),
            Column::Depth => Value::Int(i64::from(self.depth)),
            Column::Hidden => Value::Bool(self.name.as_bytes().first() == Some(&b'.')),
            Column::Kind => Value::Text(self.kind()?.as_str().to_owned()),
            Column::Size => Value::Int(i64::try_from(self.stat()?.stx_size).unwrap_or(i64::MAX)),
            Column::Blocks => {
                Value::Int(i64::try_from(self.stat()?.stx_blocks).unwrap_or(i64::MAX))
            }
            Column::Mode => Value::Int(i64::from(self.stat()?.stx_mode)),
            Column::Perms => Value::Text(perms(u32::from(self.stat()?.stx_mode))),
            Column::Setuid => Value::Bool(self.stat()?.stx_mode & 0o4000 != 0),
            Column::Setgid => Value::Bool(self.stat()?.stx_mode & 0o2000 != 0),
            Column::Sticky => Value::Bool(self.stat()?.stx_mode & 0o1000 != 0),
            Column::Uid => Value::Int(i64::from(self.stat()?.stx_uid)),
            Column::Gid => Value::Int(i64::from(self.stat()?.stx_gid)),
            Column::User => self
                .shared
                .user_name(self.stat()?.stx_uid)
                .map(|name| from_bytes(name.as_bytes()))
                .unwrap_or(Value::Null),
            Column::Group => self
                .shared
                .group_name(self.stat()?.stx_gid)
                .map(|name| from_bytes(name.as_bytes()))
                .unwrap_or(Value::Null),
            Column::Nlink => Value::Int(i64::from(self.stat()?.stx_nlink)),
            Column::Inode => Value::Int(i64::try_from(self.stat()?.stx_ino).unwrap_or(i64::MAX)),
            Column::Dev => {
                let stat = self.stat()?;
                Value::Int(
                    i64::try_from(makedev(stat.stx_dev_major, stat.stx_dev_minor))
                        .unwrap_or(i64::MAX),
                )
            }
            Column::Atime => {
                let t = self.stat()?.stx_atime;
                timestamp(t.tv_sec, t.tv_nsec)
            }
            Column::Mtime => {
                let t = self.stat()?.stx_mtime;
                timestamp(t.tv_sec, t.tv_nsec)
            }
            Column::Ctime => {
                let t = self.stat()?.stx_ctime;
                timestamp(t.tv_sec, t.tv_nsec)
            }
            Column::Btime => {
                let stat = self.stat()?;
                if stat.stx_mask & StatxFlags::BTIME.bits() != 0 {
                    timestamp(stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec)
                } else {
                    Value::Null
                }
            }
            Column::Target => self.target()?.map(from_bytes).unwrap_or(Value::Null),
            Column::Broken => self.broken()?.map(Value::Bool).unwrap_or(Value::Null),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perms_render_like_ls() {
        assert_eq!(perms(0o755), "rwxr-xr-x");
        assert_eq!(perms(0o644), "rw-r--r--");
        assert_eq!(perms(0o4755), "rwsr-xr-x");
        assert_eq!(perms(0o4644), "rwSr--r--");
        assert_eq!(perms(0o2755), "rwxr-sr-x");
        assert_eq!(perms(0o1777), "rwxrwxrwt");
        assert_eq!(perms(0o1776), "rwxrwxrwT");
    }

    #[test]
    fn makedev_matches_the_kernel_encoding() {
        assert_eq!(makedev(8, 1), 0x801);
        assert_eq!(makedev(259, 0), 0x10300);
        assert_eq!(makedev(0x1234, 0x56789a), 0x1005_6782_349a);
        assert_eq!(rustix::fs::major(makedev(0x1234, 0x56789a)), 0x1234);
        assert_eq!(rustix::fs::minor(makedev(0x1234, 0x56789a)), 0x56789a);
    }
}
