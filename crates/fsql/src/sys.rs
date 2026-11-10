use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::row::Kind;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub mode: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: u64,
    pub blocks: Option<u64>,
    pub nlink: u64,
    pub dev: u64,
    pub ino: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: Option<i64>,
    pub btime: Option<i64>,
    pub mount_id: Option<u64>,
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Owner {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

pub fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn nul(path: &Path) -> Error {
    io(path, std::io::Error::other("path contains a NUL byte"))
}

pub fn name_of(name: &OsStr, path: &Path) -> Result<Name> {
    Name::new(name).ok_or_else(|| nul(path))
}

pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(bytes))
}

pub fn path_bytes(path: &Path) -> Vec<u8> {
    os_bytes(path.as_os_str())
}

pub fn is_cross_device(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::CrossesDevices
}

pub fn split(path: &Path) -> Result<(Dir, Name)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Error::Plan(format!("`{}` has no parent", path.display())));
    };
    Ok((open_chain(parent)?, name_of(name, path)?))
}

pub fn file_name(name: &[u8]) -> Option<OsString> {
    let valid = !name.is_empty()
        && name != b"."
        && name != b".."
        && !name
            .iter()
            .any(|byte| SEPARATORS.contains(byte) || *byte == 0);
    valid.then(|| os_from_bytes(name))
}
