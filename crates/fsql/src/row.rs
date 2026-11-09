use std::cell::OnceCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tree_fucker::fs::Anchor;
use tree_fucker::std_fs::DirectoryAnchor;

use crate::column::Column;
use crate::error::{Error, Result};
use crate::eval::{Row, from_bytes};
use crate::sys::{self, Accounts, Name, Stat};
use crate::value::{Nanos, Value};

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
    pub fn from_mode(mode: u32) -> Self {
        match mode & 0o170000 {
            0o100000 => Self::File,
            0o040000 => Self::Dir,
            0o120000 => Self::Symlink,
            0o010000 => Self::Fifo,
            0o140000 => Self::Socket,
            0o060000 => Self::Block,
            0o020000 => Self::Char,
            _ => Self::Unknown,
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

    pub fn of(stat: &Stat) -> Self {
        Self {
            dev: stat.dev,
            ino: stat.ino,
            ctime: stat.ctime.unwrap_or(stat.mtime),
        }
    }
}

#[cfg(target_os = "linux")]
pub fn makedev(major: u32, minor: u32) -> u64 {
    rustix::fs::makedev(major, minor)
}

#[derive(Default)]
pub struct Shared {
    accounts: Accounts,
}

impl Shared {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
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
    anchor: Option<Anchor>,
    name: Name,
    path: PathBuf,
    source_root: Rc<Path>,
    depth: u32,
    kind_hint: Kind,
    stat: OnceCell<std::result::Result<Stat, std::io::ErrorKind>>,
    target: OnceCell<Option<Vec<u8>>>,
}

impl Entry {
    pub fn new(
        shared: Rc<Shared>,
        anchor: Option<Anchor>,
        path: PathBuf,
        depth: u32,
        kind_hint: Kind,
    ) -> Result<Self> {
        let root = path
            .ancestors()
            .nth(depth as usize)
            .unwrap_or_else(|| sys::root(&path))
            .to_path_buf();
        Self::rooted(shared, anchor, path, &Rc::from(root), depth, kind_hint)
    }

    pub(crate) fn rooted(
        shared: Rc<Shared>,
        anchor: Option<Anchor>,
        path: PathBuf,
        source_root: &Rc<Path>,
        depth: u32,
        kind_hint: Kind,
    ) -> Result<Self> {
        if !path.starts_with(source_root) {
            return Err(Error::Plan("entry is outside its source root".into()));
        }
        let name = sys::name_of(path.file_name().unwrap_or(OsStr::new(".")), &path)?;
        Ok(Self {
            shared,
            anchor,
            name,
            path,
            source_root: source_root.clone(),
            depth,
            kind_hint,
            stat: OnceCell::new(),
            target: OnceCell::new(),
        })
    }

    pub fn name(&self) -> &OsStr {
        self.name.as_os_str()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path relative to the original source root, empty for the root row.
    pub fn relative_path(&self) -> &Path {
        self.path
            .strip_prefix(&self.source_root)
            .unwrap_or(Path::new(""))
    }

    pub fn anchored(&self) -> bool {
        self.directory().is_some()
    }

    fn directory(&self) -> Option<&DirectoryAnchor> {
        self.anchor.as_ref()?.get::<DirectoryAnchor>()
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn stat(&self) -> Result<&Stat> {
        self.stat
            .get_or_init(|| {
                sys::stat_entry(self.directory(), &self.name, &self.path, false)
                    .map_err(|error| error.kind())
            })
            .as_ref()
            .map_err(|kind| Error::Io {
                path: self.path.clone(),
                source: std::io::Error::from(*kind),
            })
    }

    pub fn kind(&self) -> Result<Kind> {
        if self.kind_hint != Kind::Unknown {
            return Ok(self.kind_hint);
        }
        Ok(self.stat()?.kind)
    }

    pub fn is_dir(&self) -> Result<bool> {
        Ok(self.kind()? == Kind::Dir)
    }

    pub fn mount_id(&self) -> Result<Option<u64>> {
        Ok(self.stat()?.mount_id)
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
        let target = self
            .target
            .get_or_init(|| sys::readlink_entry(self.directory(), &self.name, &self.path).ok());
        Ok(target.as_deref())
    }

    fn broken(&self) -> Result<Option<bool>> {
        if self.kind()? != Kind::Symlink {
            return Ok(None);
        }
        let followed = sys::stat_entry(self.directory(), &self.name, &self.path, true);
        Ok(Some(followed.is_err()))
    }
}

fn os(text: &OsStr) -> Value {
    from_bytes(&sys::os_bytes(text))
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

fn timestamp(nanos: i64) -> Value {
    Value::Timestamp(Nanos(nanos))
}

fn int(value: u64) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn optional(value: Option<u64>) -> Value {
    value.map(int).unwrap_or(Value::Null)
}

impl Row for Entry {
    fn column(&self, name: &str) -> Result<Value> {
        let column = Column::parse(name).ok_or_else(|| Error::UnknownColumn(name.to_owned()))?;
        Ok(match column {
            Column::RelativePath => from_bytes(&sys::slash_bytes(self.relative_path())),
            Column::Path => os(self.path.as_os_str()),
            Column::Name => os(self.name()),
            Column::Parent => self
                .path
                .parent()
                .map(|p| os(p.as_os_str()))
                .unwrap_or(Value::Null),
            Column::Ext => Path::new(self.name())
                .extension()
                .map(|ext| {
                    let bytes = sys::os_bytes(ext);
                    match std::str::from_utf8(&bytes) {
                        Ok(s) => Value::Text(s.to_lowercase()),
                        Err(_) => Value::Blob(bytes.to_ascii_lowercase()),
                    }
                })
                .unwrap_or(Value::Null),
            Column::Depth => Value::Int(i64::from(self.depth)),
            Column::Hidden => Value::Bool(
                sys::os_bytes(self.name()).first() == Some(&b'.')
                    || (sys::HIDDEN_ATTRIBUTE && self.stat()?.hidden),
            ),
            Column::Kind => Value::Text(self.kind()?.as_str().to_owned()),
            Column::Size => int(self.stat()?.size),
            Column::Blocks => optional(self.stat()?.blocks),
            Column::Mode => Value::Int(i64::from(self.stat()?.mode)),
            Column::Perms => Value::Text(perms(self.stat()?.mode)),
            Column::Setuid => Value::Bool(self.stat()?.mode & 0o4000 != 0),
            Column::Setgid => Value::Bool(self.stat()?.mode & 0o2000 != 0),
            Column::Sticky => Value::Bool(self.stat()?.mode & 0o1000 != 0),
            Column::Uid => optional(self.stat()?.uid.map(u64::from)),
            Column::Gid => optional(self.stat()?.gid.map(u64::from)),
            Column::User => self
                .shared
                .accounts
                .user(&self.path, self.stat()?)
                .map(|name| from_bytes(&name))
                .unwrap_or(Value::Null),
            Column::Group => self
                .shared
                .accounts
                .group(&self.path, self.stat()?)
                .map(|name| from_bytes(&name))
                .unwrap_or(Value::Null),
            Column::Nlink => int(self.stat()?.nlink),
            Column::Inode => int(self.stat()?.ino),
            Column::Dev => int(self.stat()?.dev),
            Column::Atime => timestamp(self.stat()?.atime),
            Column::Mtime => timestamp(self.stat()?.mtime),
            Column::Ctime => self.stat()?.ctime.map(timestamp).unwrap_or(Value::Null),
            Column::Btime => self.stat()?.btime.map(timestamp).unwrap_or(Value::Null),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn makedev_matches_the_kernel_encoding() {
        assert_eq!(makedev(8, 1), 0x801);
        assert_eq!(makedev(259, 0), 0x10300);
        assert_eq!(makedev(0x1234, 0x56789a), 0x1005_6782_349a);
        assert_eq!(rustix::fs::major(makedev(0x1234, 0x56789a)), 0x1234);
        assert_eq!(rustix::fs::minor(makedev(0x1234, 0x56789a)), 0x56789a);
    }
}
