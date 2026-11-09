use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Dir as Entries, Gid, Mode, OFlags, Timespec, Timestamps, Uid};

use super::Stat;
use crate::error::{Error, Result};
use crate::row::Kind;
use crate::time;

pub const SEPARATORS: &[u8] = b"/";

pub const VERBATIM_PREFIXES: &[&str] = &[];

pub const HIDDEN_ATTRIBUTE: bool = false;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub struct Dir(OwnedFd);

impl Dir {
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self(fd)
    }
}

pub struct Name(CString);

impl Name {
    pub fn new(name: &OsStr) -> Option<Self> {
        CString::new(name.as_bytes()).ok().map(Self)
    }

    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(self.0.as_bytes())
    }
}

#[cfg(target_os = "linux")]
pub struct Pinned(OwnedFd);

#[cfg(not(target_os = "linux"))]
pub enum Pinned {
    Descriptor(OwnedFd),
    Name {
        dir: OwnedFd,
        name: CString,
        dev: u64,
        ino: u64,
    },
}

pub fn file_time(nanos: i64) -> i64 {
    nanos
}

#[cfg(target_os = "linux")]
pub fn identity_birth(stat: &Stat) -> Option<i64> {
    stat.btime
}

#[cfg(not(target_os = "linux"))]
pub fn identity_birth(_stat: &Stat) -> Option<i64> {
    None
}

pub fn applied_mode(_current: u32, requested: u32) -> u32 {
    requested & 0o7777
}

pub fn native_path(text: &str) -> PathBuf {
    PathBuf::from(text)
}

pub fn native_path_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(bytes))
}

pub fn os_bytes(text: &OsStr) -> Vec<u8> {
    text.as_bytes().to_vec()
}

pub fn slash_bytes(path: &Path) -> Vec<u8> {
    os_bytes(path.as_os_str())
}

pub fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

pub fn root(path: &Path) -> &Path {
    path.ancestors().last().unwrap_or(Path::new("/"))
}

fn open_dir(dir: impl rustix::fd::AsFd, name: &CString, path: &Path) -> Result<OwnedFd> {
    rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty()).map_err(|e| super::io(path, e.into()))
}

pub fn open_chain(directory: &Path) -> Result<Dir> {
    crate::walk::validate_path(directory)?;
    let mut fd = open_dir(CWD, &c"/".to_owned(), Path::new("/"))?;
    let mut opened = PathBuf::from("/");
    for component in directory.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        opened.push(part);
        let name = super::name_of(part, &opened)?;
        fd = open_dir(&fd, &name.0, &opened)?;
    }
    Ok(Dir(fd))
}

pub fn validate_name(path: &Path) -> Result<()> {
    Name::new(path.as_os_str())
        .map(|_| ())
        .ok_or_else(|| super::nul(path))
}

fn nanos(seconds: impl Into<i128>, nanoseconds: impl Into<i128>) -> i64 {
    let seconds = i64::try_from(seconds.into()).unwrap_or(0);
    let nanoseconds = u32::try_from(nanoseconds.into()).unwrap_or(0);
    time::from_parts(seconds, nanoseconds).0
}

#[cfg(target_os = "linux")]
mod stat {
    use rustix::fs::{AtFlags, Statx, StatxFlags};

    use super::{Kind, Stat, nanos};

    pub const MASK: StatxFlags = StatxFlags::BASIC_STATS
        .union(StatxFlags::BTIME)
        .union(StatxFlags::MNT_ID);

    pub fn convert(stat: &Statx) -> Stat {
        let mode = u32::from(stat.stx_mode);
        Stat {
            kind: Kind::from_mode(mode),
            mode,
            uid: Some(stat.stx_uid),
            gid: Some(stat.stx_gid),
            size: stat.stx_size,
            blocks: Some(stat.stx_blocks),
            nlink: u64::from(stat.stx_nlink),
            dev: rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
            ino: stat.stx_ino,
            atime: nanos(stat.stx_atime.tv_sec, stat.stx_atime.tv_nsec),
            mtime: nanos(stat.stx_mtime.tv_sec, stat.stx_mtime.tv_nsec),
            ctime: Some(nanos(stat.stx_ctime.tv_sec, stat.stx_ctime.tv_nsec)),
            btime: (stat.stx_mask & StatxFlags::BTIME.bits() != 0)
                .then(|| nanos(stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec)),
            mount_id: (stat.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id),
            hidden: false,
        }
    }

    pub fn at(
        dir: impl rustix::fd::AsFd,
        name: &std::ffi::CStr,
        follow: bool,
    ) -> rustix::io::Result<Stat> {
        let flags = if follow {
            AtFlags::empty()
        } else {
            AtFlags::SYMLINK_NOFOLLOW
        };
        rustix::fs::statx(dir, name, flags, MASK).map(|stat| convert(&stat))
    }

    pub fn fd(fd: impl rustix::fd::AsFd) -> rustix::io::Result<Stat> {
        rustix::fs::statx(
            fd,
            c"",
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
            MASK,
        )
        .map(|stat| convert(&stat))
    }
}

#[cfg(not(target_os = "linux"))]
mod stat {
    use rustix::fs::AtFlags;

    use super::{Kind, Stat, nanos};

    fn unsigned(value: impl Into<i128>) -> u64 {
        u64::try_from(value.into()).unwrap_or(0)
    }

    pub fn convert(stat: &rustix::fs::Stat) -> Stat {
        let mode = u32::try_from(unsigned(stat.st_mode)).unwrap_or(0);
        Stat {
            kind: Kind::from_mode(mode),
            mode,
            uid: Some(stat.st_uid),
            gid: Some(stat.st_gid),
            size: unsigned(stat.st_size),
            blocks: Some(unsigned(stat.st_blocks)),
            nlink: unsigned(stat.st_nlink),
            dev: unsigned(stat.st_dev),
            ino: unsigned(stat.st_ino),
            atime: nanos(stat.st_atime, stat.st_atime_nsec),
            mtime: nanos(stat.st_mtime, stat.st_mtime_nsec),
            ctime: Some(nanos(stat.st_ctime, stat.st_ctime_nsec)),
            btime: birth(stat),
            mount_id: None,
            hidden: false,
        }
    }

    #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "netbsd"))]
    fn birth(stat: &rustix::fs::Stat) -> Option<i64> {
        Some(nanos(stat.st_birthtime, stat.st_birthtime_nsec))
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "freebsd", target_os = "netbsd")))]
    fn birth(_stat: &rustix::fs::Stat) -> Option<i64> {
        None
    }

    pub fn at(
        dir: impl rustix::fd::AsFd,
        name: &std::ffi::CStr,
        follow: bool,
    ) -> rustix::io::Result<Stat> {
        let flags = if follow {
            AtFlags::empty()
        } else {
            AtFlags::SYMLINK_NOFOLLOW
        };
        rustix::fs::statat(dir, name, flags).map(|stat| convert(&stat))
    }

    pub fn fd(fd: impl rustix::fd::AsFd) -> rustix::io::Result<Stat> {
        rustix::fs::fstat(fd).map(|stat| convert(&stat))
    }
}

pub fn stat_at(dir: &Dir, name: &Name) -> io::Result<Stat> {
    Ok(stat::at(&dir.0, &name.0, false)?)
}

pub fn stat_followed_at(dir: &Dir, name: &Name) -> io::Result<Stat> {
    Ok(stat::at(&dir.0, &name.0, true)?)
}

pub fn stat_entry(
    anchor: Option<&tree_fucker::std_fs::DirectoryAnchor>,
    name: &Name,
    path: &Path,
    follow: bool,
) -> io::Result<Stat> {
    match anchor {
        Some(directory) => Ok(stat::at(directory, &name.0, follow)?),
        None => stat_path(path, follow),
    }
}

pub fn readlink_entry(
    anchor: Option<&tree_fucker::std_fs::DirectoryAnchor>,
    name: &Name,
    path: &Path,
) -> io::Result<Vec<u8>> {
    match anchor {
        Some(directory) => Ok(rustix::fs::readlinkat(directory, &name.0, Vec::new())?.into_bytes()),
        None => readlink_path(path),
    }
}

pub fn stat_path(path: &Path, follow: bool) -> io::Result<Stat> {
    let name =
        Name::new(path.as_os_str()).ok_or_else(|| io::Error::other("path contains a NUL byte"))?;
    Ok(stat::at(CWD, &name.0, follow)?)
}

pub fn stat_file(file: &File) -> io::Result<Stat> {
    Ok(stat::fd(file)?)
}

pub fn readlink_at(dir: &Dir, name: &Name) -> io::Result<Vec<u8>> {
    Ok(rustix::fs::readlinkat(&dir.0, &name.0, Vec::new())?.into_bytes())
}

pub fn readlink_path(path: &Path) -> io::Result<Vec<u8>> {
    Ok(std::fs::read_link(path)?.into_os_string().into_vec())
}

#[cfg(target_os = "linux")]
pub fn pin(dir: &Dir, name: &Name) -> io::Result<Pinned> {
    Ok(Pinned(rustix::fs::openat(
        &dir.0,
        &name.0,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

#[cfg(not(target_os = "linux"))]
fn pin_name(dir: &Dir, name: &Name) -> io::Result<Pinned> {
    let stat = stat::at(&dir.0, &name.0, false)?;
    Ok(Pinned::Name {
        dir: rustix::io::fcntl_dupfd_cloexec(&dir.0, 0)?,
        name: name.0.clone(),
        dev: stat.dev,
        ino: stat.ino,
    })
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub fn pin(dir: &Dir, name: &Name) -> io::Result<Pinned> {
    pin_name(dir, name)
}

#[cfg(target_vendor = "apple")]
pub fn pin(dir: &Dir, name: &Name) -> io::Result<Pinned> {
    let event_only = OFlags::from_bits_retain(libc::O_EVTONLY.cast_unsigned());
    match rustix::fs::openat(
        &dir.0,
        &name.0,
        event_only | OFlags::SYMLINK | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Pinned::Descriptor(fd)),
        Err(rustix::io::Errno::ACCESS) => pin_name(dir, name),
        Err(errno) => Err(errno.into()),
    }
}

#[cfg(target_os = "linux")]
pub fn pin_file(file: &File, _dir: &Dir, _name: &Name) -> io::Result<Pinned> {
    Ok(Pinned(rustix::io::fcntl_dupfd_cloexec(file, 0)?))
}

#[cfg(not(target_os = "linux"))]
pub fn pin_file(file: &File, _dir: &Dir, _name: &Name) -> io::Result<Pinned> {
    Ok(Pinned::Descriptor(rustix::io::fcntl_dupfd_cloexec(
        file, 0,
    )?))
}

#[cfg(target_os = "linux")]
pub fn relocate(pinned: &Pinned, _dir: &Dir, _name: &Name) -> io::Result<Pinned> {
    Ok(Pinned(rustix::io::fcntl_dupfd_cloexec(&pinned.0, 0)?))
}

#[cfg(not(target_os = "linux"))]
pub fn relocate(pinned: &Pinned, dir: &Dir, name: &Name) -> io::Result<Pinned> {
    Ok(match pinned {
        Pinned::Descriptor(fd) => Pinned::Descriptor(rustix::io::fcntl_dupfd_cloexec(fd, 0)?),
        Pinned::Name { dev, ino, .. } => Pinned::Name {
            dir: rustix::io::fcntl_dupfd_cloexec(&dir.0, 0)?,
            name: name.0.clone(),
            dev: *dev,
            ino: *ino,
        },
    })
}

#[cfg(target_os = "linux")]
pub fn stat_pinned(pinned: &Pinned) -> io::Result<Stat> {
    Ok(stat::fd(&pinned.0)?)
}

#[cfg(not(target_os = "linux"))]
fn named(dir: &OwnedFd, name: &CString, dev: u64, ino: u64) -> io::Result<Stat> {
    let stat = stat::at(dir, name, false)?;
    if (stat.dev, stat.ino) != (dev, ino) {
        return Err(io::Error::other("another object replaced the pinned entry"));
    }
    Ok(stat)
}

#[cfg(not(target_os = "linux"))]
pub fn stat_pinned(pinned: &Pinned) -> io::Result<Stat> {
    match pinned {
        Pinned::Descriptor(fd) => Ok(stat::fd(fd)?),
        Pinned::Name {
            dir,
            name,
            dev,
            ino,
        } => named(dir, name, *dev, *ino),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_mode(pinned: &Pinned, mode: u32) -> io::Result<()> {
    match pinned {
        Pinned::Descriptor(fd) => Ok(rustix::fs::fchmod(fd, raw_mode(mode))?),
        Pinned::Name {
            dir,
            name,
            dev,
            ino,
        } => {
            named(dir, name, *dev, *ino)?;
            Ok(rustix::fs::chmodat(
                dir,
                name,
                raw_mode(mode),
                AtFlags::SYMLINK_NOFOLLOW,
            )?)
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_owner(pinned: &Pinned, owner: &super::Owner) -> io::Result<()> {
    let uid = owner.uid.map(Uid::from_raw);
    let gid = owner.gid.map(Gid::from_raw);
    match pinned {
        Pinned::Descriptor(fd) => Ok(rustix::fs::fchown(fd, uid, gid)?),
        Pinned::Name {
            dir,
            name,
            dev,
            ino,
        } => {
            named(dir, name, *dev, *ino)?;
            Ok(rustix::fs::chownat(
                dir,
                name,
                uid,
                gid,
                AtFlags::SYMLINK_NOFOLLOW,
            )?)
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_times(pinned: &Pinned, atime: Option<i64>, mtime: Option<i64>) -> io::Result<()> {
    let times = Timestamps {
        last_access: timespec(atime),
        last_modification: timespec(mtime),
    };
    match pinned {
        Pinned::Descriptor(fd) => Ok(rustix::fs::futimens(fd, &times)?),
        Pinned::Name {
            dir,
            name,
            dev,
            ino,
        } => {
            named(dir, name, *dev, *ino)?;
            Ok(rustix::fs::utimensat(
                dir,
                name,
                &times,
                AtFlags::SYMLINK_NOFOLLOW,
            )?)
        }
    }
}

fn narrow<S, T: TryFrom<S> + Default>(value: S) -> T {
    T::try_from(value).unwrap_or_default()
}

fn raw_mode(mode: u32) -> Mode {
    Mode::from_bits_truncate(narrow(mode & 0o7777))
}

#[cfg(target_os = "linux")]
pub fn set_mode(pinned: &Pinned, mode: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // rustix's chmodat does not implement fchmodat2. This Linux procfs magic
    // link addresses our held O_PATH descriptor, never the mutable entry name.
    let descriptor = format!("/proc/self/fd/{}", pinned.0.as_raw_fd());
    Ok(rustix::fs::chmodat(
        CWD,
        descriptor.as_str(),
        raw_mode(mode),
        AtFlags::empty(),
    )?)
}

#[cfg(target_os = "linux")]
pub fn set_owner(pinned: &Pinned, owner: &super::Owner) -> io::Result<()> {
    Ok(rustix::fs::chownat(
        &pinned.0,
        c"",
        owner.uid.map(Uid::from_raw),
        owner.gid.map(Gid::from_raw),
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH,
    )?)
}

fn timespec(nanos: Option<i64>) -> Timespec {
    match nanos {
        Some(nanos) => Timespec {
            tv_sec: nanos.div_euclid(time::NANOS_PER_SECOND),
            tv_nsec: narrow(nanos.rem_euclid(time::NANOS_PER_SECOND)),
        },
        None => Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
    }
}

#[cfg(target_os = "linux")]
pub fn set_times(pinned: &Pinned, atime: Option<i64>, mtime: Option<i64>) -> io::Result<()> {
    let times = Timestamps {
        last_access: timespec(atime),
        last_modification: timespec(mtime),
    };
    Ok(rustix::fs::utimensat(
        &pinned.0,
        c"",
        &times,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH,
    )?)
}

pub fn unlink(dir: &Dir, name: &Name, kind: Kind) -> io::Result<()> {
    let flags = if kind == Kind::Dir {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    Ok(rustix::fs::unlinkat(&dir.0, &name.0, flags)?)
}

pub fn rename_noreplace(from: &Dir, from_name: &Name, to: &Dir, to_name: &Name) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(
        &from.0,
        &from_name.0,
        &to.0,
        &to_name.0,
        rustix::fs::RenameFlags::NOREPLACE,
    )?)
}

pub fn rename_replace(from: &Dir, from_name: &Name, to: &Dir, to_name: &Name) -> io::Result<()> {
    Ok(rustix::fs::renameat(
        &from.0,
        &from_name.0,
        &to.0,
        &to_name.0,
    )?)
}

pub fn symlink(target: &[u8], dir: &Dir, name: &Name, _home: &Path) -> io::Result<()> {
    Ok(rustix::fs::symlinkat(
        OsStr::from_bytes(target),
        &dir.0,
        &name.0,
    )?)
}

pub fn mkdir(dir: &Dir, name: &Name, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::mkdirat(&dir.0, &name.0, raw_mode(mode))?)
}

pub fn create(dir: &Dir, name: &Name, mode: u32) -> io::Result<File> {
    Ok(File::from(rustix::fs::openat(
        &dir.0,
        &name.0,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        raw_mode(mode),
    )?))
}

pub fn open_read(dir: &Dir, name: &Name) -> io::Result<File> {
    Ok(File::from(rustix::fs::openat(
        &dir.0,
        &name.0,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

pub fn is_empty_dir(dir: &Dir, name: &Name) -> io::Result<bool> {
    let fd = rustix::fs::openat(&dir.0, &name.0, DIR_FLAGS, Mode::empty())?;
    for entry in Entries::read_from(&fd)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
pub fn sync_entry(dir: &Dir, _name: &Name) -> io::Result<()> {
    Ok(rustix::fs::syncfs(&dir.0)?)
}

#[cfg(not(target_os = "linux"))]
pub fn sync_entry(dir: &Dir, name: &Name) -> io::Result<()> {
    match rustix::fs::openat(
        &dir.0,
        &name.0,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd).sync_all()?,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::ACCESS | rustix::io::Errno::LOOP) => {}
        Err(errno) => return Err(errno.into()),
    }
    sync_dir(dir)
}

#[cfg(target_vendor = "apple")]
pub fn sync_dir(dir: &Dir) -> io::Result<()> {
    Ok(rustix::fs::fcntl_fullfsync(&dir.0)?)
}

#[cfg(not(target_vendor = "apple"))]
pub fn sync_dir(dir: &Dir) -> io::Result<()> {
    Ok(rustix::fs::fsync(&dir.0)?)
}

pub fn sync_dir_path(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub fn copy_attributes(from: &File, from_path: &Path, to: &File) -> Result<()> {
    let stat = stat::fd(from).map_err(|e| super::io(from_path, e.into()))?;
    rustix::fs::fchmod(to, raw_mode(stat.mode)).map_err(|e| super::io(from_path, e.into()))?;
    let _ = rustix::fs::fchown(to, stat.uid.map(Uid::from_raw), stat.gid.map(Gid::from_raw));
    crate::xattr::copy(from_path, to)?;
    let times = Timestamps {
        last_access: timespec(Some(stat.atime)),
        last_modification: timespec(Some(stat.mtime)),
    };
    rustix::fs::futimens(to, &times).map_err(|e| super::io(from_path, e.into()))
}

pub fn private_dir(dir: &Dir, name: &Name) -> io::Result<()> {
    mkdir(dir, name, 0o700)
}

pub fn symlink_path(target: &[u8], link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(OsStr::from_bytes(target), link)
}

pub fn symlink_from(target: &[u8], link: &Path, _home: &Path) -> io::Result<()> {
    symlink_path(target, link)
}

pub fn not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

pub fn user_id(name: &[u8]) -> Result<u32> {
    uzers::get_user_by_name(OsStr::from_bytes(name))
        .map(|user| user.uid())
        .ok_or_else(|| Error::Plan(format!("unknown user `{}`", String::from_utf8_lossy(name))))
}

pub fn group_id(name: &[u8]) -> Result<u32> {
    uzers::get_group_by_name(OsStr::from_bytes(name))
        .map(|group| group.gid())
        .ok_or_else(|| Error::Plan(format!("unknown group `{}`", String::from_utf8_lossy(name))))
}

#[derive(Default)]
pub struct Accounts(uzers::UsersCache);

impl Accounts {
    pub fn user(&self, _path: &Path, stat: &Stat) -> Option<Vec<u8>> {
        use uzers::Users;
        self.0
            .get_user_by_uid(stat.uid?)
            .map(|user| user.name().as_bytes().to_vec())
    }

    pub fn group(&self, _path: &Path, stat: &Stat) -> Option<Vec<u8>> {
        use uzers::Groups;
        self.0
            .get_group_by_gid(stat.gid?)
            .map(|group| group.name().as_bytes().to_vec())
    }
}

pub fn anchor_limit() -> usize {
    use rustix::process::{Resource, getrlimit};
    let open_files = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
    usize::try_from(open_files / 4).unwrap_or(usize::MAX)
}

pub fn effective_root() -> bool {
    rustix::process::geteuid().is_root()
}

const XATTR_MAX: usize = 65_536;

#[cfg(target_os = "linux")]
fn xattr_absent(errno: rustix::io::Errno) -> bool {
    use rustix::io::Errno;
    errno == Errno::NODATA || errno == Errno::OPNOTSUPP || errno == Errno::NOTSUP
}

#[cfg(not(target_os = "linux"))]
fn xattr_absent(errno: rustix::io::Errno) -> bool {
    use rustix::io::Errno;
    errno == Errno::NOATTR || errno == Errno::OPNOTSUPP || errno == Errno::NOTSUP
}

pub fn xattr_names(path: &Path) -> Result<Vec<Vec<u8>>> {
    let mut buffer = vec![0u8; XATTR_MAX];
    let len = match rustix::fs::llistxattr(path, &mut buffer[..]) {
        Ok(len) => len,
        Err(errno) if xattr_absent(errno) => return Ok(Vec::new()),
        Err(errno) => return Err(super::io(path, errno.into())),
    };
    Ok(buffer[..len]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

pub fn xattr_value(path: &Path, name: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut buffer = vec![0u8; XATTR_MAX];
    match rustix::fs::lgetxattr(path, OsStr::from_bytes(name), &mut buffer[..]) {
        Ok(len) => {
            buffer.truncate(len);
            Ok(Some(buffer))
        }
        Err(errno) if xattr_absent(errno) => Ok(None),
        Err(errno) => Err(super::io(path, errno.into())),
    }
}

pub struct Acl {
    pub kind: &'static str,
    pub tag: String,
    pub qualifier: crate::value::Value,
    pub perms: String,
}

#[cfg(target_os = "linux")]
pub use posix_acl::acl_entries;

#[cfg(target_os = "linux")]
mod posix_acl {
    use super::{Acl, xattr_value};
    use crate::error::Result;
    use crate::eval::from_bytes;
    use crate::row::{Entry, Kind};
    use crate::value::Value;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AclEntry {
        pub tag: u16,
        pub perm: u16,
        pub id: u32,
    }

    const ACL_VERSION: u32 = 2;
    const ACL_USER_OBJ: u16 = 0x01;
    const ACL_USER: u16 = 0x02;
    const ACL_GROUP_OBJ: u16 = 0x04;
    const ACL_GROUP: u16 = 0x08;
    const ACL_MASK: u16 = 0x10;
    const ACL_OTHER: u16 = 0x20;
    const ACL_UNDEFINED_ID: u32 = u32::MAX;

    pub fn parse_acl(bytes: &[u8]) -> Option<Vec<AclEntry>> {
        let (header, body) = bytes.split_at_checked(4)?;
        if u32::from_le_bytes(header.try_into().ok()?) != ACL_VERSION {
            return None;
        }
        body.chunks(8)
            .map(|chunk| {
                let tag = u16::from_le_bytes(chunk.get(0..2)?.try_into().ok()?);
                let perm = u16::from_le_bytes(chunk.get(2..4)?.try_into().ok()?);
                let id = u32::from_le_bytes(chunk.get(4..8)?.try_into().ok()?);
                Some(AclEntry { tag, perm, id })
            })
            .collect()
    }

    pub fn perms(perm: u16) -> String {
        let mut out = String::with_capacity(3);
        out.push(if perm & 4 != 0 { 'r' } else { '-' });
        out.push(if perm & 2 != 0 { 'w' } else { '-' });
        out.push(if perm & 1 != 0 { 'x' } else { '-' });
        out
    }

    fn qualifier(entry: &AclEntry) -> Value {
        use std::os::unix::ffi::OsStrExt;
        if entry.id == ACL_UNDEFINED_ID {
            return Value::Null;
        }
        let name = match entry.tag {
            ACL_USER => uzers::get_user_by_uid(entry.id).map(|u| u.name().to_os_string()),
            ACL_GROUP => uzers::get_group_by_gid(entry.id).map(|g| g.name().to_os_string()),
            _ => None,
        };
        match name {
            Some(name) => from_bytes(name.as_bytes()),
            None => match entry.tag {
                ACL_USER | ACL_GROUP => Value::Int(i64::from(entry.id)),
                _ => Value::Null,
            },
        }
    }

    pub fn tag_name(tag: u16) -> &'static str {
        match tag {
            ACL_USER_OBJ | ACL_USER => "user",
            ACL_GROUP_OBJ | ACL_GROUP => "group",
            ACL_MASK => "mask",
            ACL_OTHER => "other",
            _ => "unknown",
        }
    }

    pub fn acl_entries(entry: &Entry) -> Result<Vec<Acl>> {
        let path = entry.path();
        let mut kinds = vec!["access"];
        if entry.kind().ok() == Some(Kind::Dir) {
            kinds.push("default");
        }
        let mut out = Vec::new();
        for kind in kinds {
            let attribute = format!("system.posix_acl_{kind}");
            let Some(bytes) = xattr_value(path, attribute.as_bytes())? else {
                continue;
            };
            let Some(entries) = parse_acl(&bytes) else {
                continue;
            };
            out.extend(entries.iter().map(|acl| Acl {
                kind,
                tag: tag_name(acl.tag).to_owned(),
                qualifier: qualifier(acl),
                perms: perms(acl.perm),
            }));
        }
        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_a_posix_acl_blob() {
            let mut blob = ACL_VERSION.to_le_bytes().to_vec();
            for (tag, perm, id) in [
                (ACL_USER_OBJ, 6u16, ACL_UNDEFINED_ID),
                (ACL_USER, 4u16, 1000u32),
                (ACL_GROUP_OBJ, 4u16, ACL_UNDEFINED_ID),
                (ACL_MASK, 4u16, ACL_UNDEFINED_ID),
                (ACL_OTHER, 0u16, ACL_UNDEFINED_ID),
            ] {
                blob.extend_from_slice(&tag.to_le_bytes());
                blob.extend_from_slice(&perm.to_le_bytes());
                blob.extend_from_slice(&id.to_le_bytes());
            }
            let entries = parse_acl(&blob).expect("acl");
            assert_eq!(entries.len(), 5);
            assert_eq!(
                entries[1],
                AclEntry {
                    tag: ACL_USER,
                    perm: 4,
                    id: 1000
                }
            );
            assert_eq!(perms(6), "rw-");
            assert_eq!(perms(0), "---");
            assert_eq!(tag_name(ACL_MASK), "mask");
            assert_eq!(parse_acl(&[1, 0, 0, 0]), None);
            assert_eq!(parse_acl(&[]), None);
        }
    }
}

#[cfg(target_os = "freebsd")]
fn acl_kind(flags: exacl::Flag) -> &'static str {
    if flags.contains(exacl::Flag::DEFAULT) {
        "default"
    } else {
        "access"
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn acl_kind(_flags: exacl::Flag) -> &'static str {
    "access"
}

#[cfg(not(target_os = "linux"))]
pub fn acl_entries(entry: &crate::row::Entry) -> Result<Vec<Acl>> {
    use crate::value::Value;
    let path = entry.path();
    let entries = match exacl::getfacl(path, exacl::AclOption::SYMLINK_ACL) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::Unsupported => return Ok(Vec::new()),
        Err(error) => return Err(super::io(path, error)),
    };
    Ok(entries
        .into_iter()
        .map(|acl| Acl {
            kind: acl_kind(acl.flags),
            tag: format!("{:?}", acl.kind).to_ascii_lowercase(),
            qualifier: if acl.name.is_empty() {
                Value::Null
            } else {
                Value::Text(acl.name)
            },
            perms: format!(
                "{}{}",
                if acl.allow { "allow:" } else { "deny:" },
                acl.perms
            ),
        })
        .collect())
}
