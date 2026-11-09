use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, FileTimes, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileTimesExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use windows_permissions::constants::{AceType, SeObjectType, SecurityInformation};
use windows_permissions::wrappers::{GetSecurityInfo, LookupAccountSid};
use windows_permissions::{LocalBox, SecurityDescriptor, Sid};
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, READ_CONTROL, SYNCHRONIZE,
};

use super::Stat;
use crate::error::{Error, Result};
use crate::row::Kind;
use crate::value::Value;

pub const SEPARATORS: &[u8] = b"/\\:";

pub const VERBATIM_PREFIXES: &[&str] = &[r"\\?\"];

pub const HIDDEN_ATTRIBUTE: bool = true;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

const SHARE_UNMOVABLE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

const NO_FOLLOW: u32 = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;

const NANOS_PER_TICK: i64 = 100;

const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

pub struct Dir {
    path: PathBuf,
    _chain: Vec<File>,
}

pub struct Name(OsString);

impl Name {
    pub fn new(name: &OsStr) -> Option<Self> {
        (!name.encode_wide().any(|unit| unit == 0)).then(|| Self(name.to_os_string()))
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

pub struct Pinned(File);

fn child(dir: &Dir, name: &Name) -> PathBuf {
    dir.path.join(&name.0)
}

pub fn identity_birth(_stat: &Stat) -> Option<i64> {
    None
}

pub fn file_time(nanos: i64) -> i64 {
    nanos / NANOS_PER_TICK * NANOS_PER_TICK
}

pub fn applied_mode(current: u32, requested: u32) -> u32 {
    let base = if requested & 0o222 == 0 { 0o444 } else { 0o666 };
    (current & 0o111) | base
}

pub fn native_path(text: &str) -> PathBuf {
    PathBuf::from(text.replace('/', "\\"))
}

pub fn native_path_bytes(bytes: &[u8]) -> PathBuf {
    let native: Vec<u8> = bytes
        .iter()
        .map(|byte| if *byte == b'/' { b'\\' } else { *byte })
        .collect();
    PathBuf::from(os_from_bytes(&native))
}

pub fn os_bytes(text: &OsStr) -> Vec<u8> {
    text.as_encoded_bytes().to_vec()
}

pub fn slash_bytes(path: &Path) -> Vec<u8> {
    os_bytes(path.as_os_str())
        .into_iter()
        .map(|byte| if byte == b'\\' { b'/' } else { byte })
        .collect()
}

pub fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_wide(&wtf8_units(bytes))
}

fn wtf8_units(bytes: &[u8]) -> Vec<u16> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match decode_point(&bytes[index..]) {
            Some((point, length)) => {
                index += length;
                push_point(&mut units, point);
            }
            None => {
                index += 1;
                units.push(0xFFFD);
            }
        }
    }
    units
}

fn decode_point(bytes: &[u8]) -> Option<(u32, usize)> {
    let lead = *bytes.first()?;
    let (length, initial) = match lead {
        0x00..=0x7F => return Some((u32::from(lead), 1)),
        0xC2..=0xDF => (2, u32::from(lead & 0x1F)),
        0xE0..=0xEF => (3, u32::from(lead & 0x0F)),
        0xF0..=0xF4 => (4, u32::from(lead & 0x07)),
        _ => return None,
    };
    let tail = bytes.get(1..length)?;
    let point = tail.iter().try_fold(initial, |point, byte| {
        (byte & 0xC0 == 0x80).then(|| (point << 6) | u32::from(byte & 0x3F))
    })?;
    let minimum = match length {
        2 => 0x80,
        3 => 0x800,
        _ => 0x10000,
    };
    (point >= minimum && point <= 0x10FFFF).then_some((point, length))
}

fn push_point(units: &mut Vec<u16>, point: u32) {
    match u16::try_from(point) {
        Ok(unit) => units.push(unit),
        Err(_) => {
            let offset = point - 0x10000;
            units.extend(
                [0xD800 | (offset >> 10), 0xDC00 | (offset & 0x3FF)]
                    .into_iter()
                    .filter_map(|unit| u16::try_from(unit).ok()),
            );
        }
    }
}

pub fn root(path: &Path) -> &Path {
    path.ancestors().last().unwrap_or(path)
}

fn open_shared(path: &Path, access: u32, share: u32, follow: bool) -> io::Result<File> {
    let flags = if follow {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        NO_FOLLOW
    };
    OpenOptions::new()
        .access_mode(access)
        .share_mode(share)
        .custom_flags(flags)
        .open(path)
}

fn open_handle(path: &Path, access: u32, follow: bool) -> io::Result<File> {
    open_shared(path, access, SHARE_ALL, follow)
}

fn open_directory(path: &Path) -> Result<File> {
    // NTFS ignores the share mode of attribute-only opens.
    let handle = open_shared(
        path,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_UNMOVABLE,
        false,
    )
    .map_err(|e| super::io(path, e))?;
    let attributes = handle
        .metadata()
        .map_err(|e| super::io(path, e))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(super::io(
            path,
            io::Error::other("refusing to follow a reparse point"),
        ));
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(super::io(path, io::ErrorKind::NotADirectory.into()));
    }
    Ok(handle)
}

pub fn open_chain(directory: &Path) -> Result<Dir> {
    crate::walk::validate_path(directory)?;
    let mut opened = PathBuf::new();
    let mut chain = Vec::new();
    for component in directory.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => opened.push(component),
            Component::Normal(part) => {
                opened.push(part);
                chain.push(open_directory(&opened)?);
            }
            Component::CurDir | Component::ParentDir => {}
        }
    }
    if chain.is_empty() {
        chain.push(open_directory(&opened)?);
    }
    Ok(Dir {
        path: opened,
        _chain: chain,
    })
}

pub fn validate_name(path: &Path) -> Result<()> {
    Name::new(path.as_os_str())
        .map(|_| ())
        .ok_or_else(|| super::nul(path))
}

fn ticks_to_nanos(ticks: u64) -> i64 {
    i64::try_from(ticks)
        .unwrap_or(i64::MAX)
        .saturating_sub(UNIX_EPOCH_TICKS)
        .saturating_mul(NANOS_PER_TICK)
}

fn stat_handle(file: &File) -> io::Result<Stat> {
    let metadata = file.metadata()?;
    let information = winapi_util::file::information(file)?;
    let attributes = metadata.file_attributes();
    let kind = if metadata.file_type().is_symlink() {
        Kind::Symlink
    } else if metadata.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    let readonly = attributes & FILE_ATTRIBUTE_READONLY != 0;
    let permissions = match (kind, readonly) {
        (Kind::Symlink, _) => 0o777,
        (_, false) => 0o666,
        (_, true) => 0o444,
    } | if kind == Kind::Dir { 0o111 } else { 0 };
    let kind_bits = match kind {
        Kind::Dir => 0o040000,
        Kind::Symlink => 0o120000,
        _ => 0o100000,
    };
    Ok(Stat {
        kind,
        mode: kind_bits | permissions,
        uid: None,
        gid: None,
        size: metadata.file_size(),
        blocks: None,
        nlink: information.number_of_links(),
        dev: information.volume_serial_number(),
        ino: information.file_index(),
        atime: ticks_to_nanos(metadata.last_access_time()),
        mtime: ticks_to_nanos(metadata.last_write_time()),
        ctime: None,
        btime: Some(ticks_to_nanos(metadata.creation_time())),
        mount_id: None,
        hidden: attributes & FILE_ATTRIBUTE_HIDDEN != 0,
    })
}

pub fn stat_path(path: &Path, follow: bool) -> io::Result<Stat> {
    stat_handle(&open_handle(path, FILE_READ_ATTRIBUTES, follow)?)
}

pub fn stat_at(dir: &Dir, name: &Name) -> io::Result<Stat> {
    stat_path(&child(dir, name), false)
}

pub fn stat_entry(
    _anchor: Option<&tree_fucker::std_fs::DirectoryAnchor>,
    _name: &Name,
    path: &Path,
    follow: bool,
) -> io::Result<Stat> {
    stat_path(path, follow)
}

pub fn stat_file(file: &File) -> io::Result<Stat> {
    stat_handle(file)
}

pub fn readlink_path(path: &Path) -> io::Result<Vec<u8>> {
    Ok(os_bytes(std::fs::read_link(path)?.as_os_str()))
}

pub fn readlink_at(dir: &Dir, name: &Name) -> io::Result<Vec<u8>> {
    readlink_path(&child(dir, name))
}

pub fn readlink_entry(
    _anchor: Option<&tree_fucker::std_fs::DirectoryAnchor>,
    _name: &Name,
    path: &Path,
) -> io::Result<Vec<u8>> {
    readlink_path(path)
}

pub fn pin(dir: &Dir, name: &Name) -> io::Result<Pinned> {
    Ok(Pinned(open_handle(
        &child(dir, name),
        FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
        false,
    )?))
}

pub fn pin_file(file: &File, _dir: &Dir, _name: &Name) -> io::Result<Pinned> {
    Ok(Pinned(file.try_clone()?))
}

pub fn relocate(pinned: &Pinned, _dir: &Dir, _name: &Name) -> io::Result<Pinned> {
    Ok(Pinned(pinned.0.try_clone()?))
}

pub fn stat_pinned(pinned: &Pinned) -> io::Result<Stat> {
    stat_handle(&pinned.0)
}

pub fn set_mode(pinned: &Pinned, mode: u32) -> io::Result<()> {
    let mut permissions = pinned.0.metadata()?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    pinned.0.set_permissions(permissions)
}

pub fn set_owner(_pinned: &Pinned, owner: &super::Owner) -> io::Result<()> {
    if owner.uid.is_none() && owner.gid.is_none() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows files have security identifiers, not numeric owners",
    ))
}

fn system_time(nanos: i64) -> SystemTime {
    let offset = Duration::from_nanos(nanos.unsigned_abs());
    if nanos >= 0 {
        SystemTime::UNIX_EPOCH + offset
    } else {
        SystemTime::UNIX_EPOCH - offset
    }
}

pub fn set_times(pinned: &Pinned, atime: Option<i64>, mtime: Option<i64>) -> io::Result<()> {
    let mut times = FileTimes::new();
    if let Some(atime) = atime {
        times = times.set_accessed(system_time(atime));
    }
    if let Some(mtime) = mtime {
        times = times.set_modified(system_time(mtime));
    }
    pinned.0.set_times(times)
}

fn is_directory_link(path: &Path) -> io::Result<bool> {
    Ok(std::fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0)
}

pub fn unlink(dir: &Dir, name: &Name, kind: Kind) -> io::Result<()> {
    let path = child(dir, name);
    let directory = match kind {
        Kind::Dir => true,
        Kind::Symlink => is_directory_link(&path)?,
        _ => false,
    };
    if directory {
        std::fs::remove_dir(&path)
    } else {
        std::fs::remove_file(&path)
    }
}

pub fn rename_noreplace(from: &Dir, from_name: &Name, to: &Dir, to_name: &Name) -> io::Result<()> {
    renamore::rename_exclusive(child(from, from_name), child(to, to_name))
}

pub fn rename_replace(from: &Dir, from_name: &Name, to: &Dir, to_name: &Name) -> io::Result<()> {
    let source = child(from, from_name);
    let destination = child(to, to_name);
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink_dir() => {
            replace_directory_link(&source, &destination)
        }
        _ => std::fs::rename(source, destination),
    }
}

fn replace_directory_link(source: &Path, destination: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let aside = destination.with_file_name(format!(
        ".fsql-replaced-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    renamore::rename_exclusive(destination, &aside)?;
    if let Err(error) = renamore::rename_exclusive(source, destination) {
        renamore::rename_exclusive(&aside, destination)?;
        return Err(error);
    }
    std::fs::remove_dir(&aside)
}

pub fn symlink_path(target: &[u8], link: &Path) -> io::Result<()> {
    symlink_from(target, link, link.parent().unwrap_or(link))
}

pub fn symlink_from(target: &[u8], link: &Path, home: &Path) -> io::Result<()> {
    let target = PathBuf::from(os_from_bytes(target));
    if std::fs::metadata(home.join(&target)).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(&target, link)
    } else {
        std::os::windows::fs::symlink_file(&target, link)
    }
}

pub fn symlink(target: &[u8], dir: &Dir, name: &Name, home: &Path) -> io::Result<()> {
    symlink_from(target, &child(dir, name), home)
}

fn apply_readonly(path: &Path, mode: u32) -> io::Result<()> {
    if mode & 0o222 != 0 {
        return Ok(());
    }
    let mut permissions = std::fs::symlink_metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
}

pub fn mkdir(dir: &Dir, name: &Name, mode: u32) -> io::Result<()> {
    let path = child(dir, name);
    std::fs::create_dir(&path)?;
    apply_readonly(&path, mode)
}

pub fn create(dir: &Dir, name: &Name, mode: u32) -> io::Result<File> {
    let attributes = if mode & 0o222 == 0 {
        FILE_ATTRIBUTE_READONLY
    } else {
        0
    };
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .attributes(attributes)
        .open(child(dir, name))
}

pub fn open_read(dir: &Dir, name: &Name) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(child(dir, name))
}

pub fn is_empty_dir(dir: &Dir, name: &Name) -> io::Result<bool> {
    Ok(std::fs::read_dir(child(dir, name))?.next().is_none())
}

pub fn sync_dir_path(path: &Path) -> io::Result<()> {
    open_handle(path, GENERIC_WRITE, false)?.sync_all()
}

pub fn sync_dir(dir: &Dir) -> io::Result<()> {
    sync_dir_path(&dir.path)
}

pub fn sync_entry(dir: &Dir, name: &Name) -> io::Result<()> {
    match open_handle(&child(dir, name), GENERIC_WRITE, false) {
        Ok(file) => file.sync_all()?,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) => {}
        Err(error) => return Err(error),
    }
    sync_dir(dir)
}

pub fn set_private(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn copy_attributes(from: &File, from_path: &Path, to: &File) -> Result<()> {
    let metadata = from.metadata().map_err(|e| super::io(from_path, e))?;
    let mut times = FileTimes::new();
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = metadata.modified() {
        times = times.set_modified(modified);
    }
    if let Ok(created) = metadata.created() {
        times = times.set_created(created);
    }
    to.set_times(times).map_err(|e| super::io(from_path, e))?;
    to.set_permissions(metadata.permissions())
        .map_err(|e| super::io(from_path, e))
}

pub fn not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

fn unsupported_owner(what: &str, name: &[u8]) -> Error {
    Error::Unsupported(format!(
        "setting the {what} `{}` on Windows",
        String::from_utf8_lossy(name)
    ))
}

pub fn user_id(name: &[u8]) -> Result<u32> {
    Err(unsupported_owner("user", name))
}

pub fn group_id(name: &[u8]) -> Result<u32> {
    Err(unsupported_owner("group", name))
}

fn security(
    path: &Path,
    information: SecurityInformation,
) -> io::Result<LocalBox<SecurityDescriptor>> {
    let file = open_handle(path, READ_CONTROL, false)?;
    GetSecurityInfo(&file, SeObjectType::SE_FILE_OBJECT, information)
}

fn account(sid: &Sid) -> String {
    match LookupAccountSid(sid) {
        Ok((name, domain)) if domain.is_empty() => name.to_string_lossy().into_owned(),
        Ok((name, domain)) => format!("{}\\{}", domain.to_string_lossy(), name.to_string_lossy()),
        Err(_) => sid.to_string(),
    }
}

#[derive(Default)]
pub struct Accounts(RefCell<HashMap<String, String>>);

impl Accounts {
    fn name(&self, sid: &Sid) -> String {
        let key = sid.to_string();
        self.0
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| account(sid))
            .clone()
    }

    pub fn user(&self, path: &Path, _stat: &Stat) -> Option<Vec<u8>> {
        let descriptor = security(path, SecurityInformation::Owner).ok()?;
        Some(self.name(descriptor.owner()?).into_bytes())
    }

    pub fn group(&self, path: &Path, _stat: &Stat) -> Option<Vec<u8>> {
        let descriptor = security(path, SecurityInformation::Group).ok()?;
        Some(self.name(descriptor.group()?).into_bytes())
    }
}

pub fn anchor_limit() -> usize {
    usize::MAX
}

pub fn xattr_names(path: &Path) -> Result<Vec<Vec<u8>>> {
    Err(Error::Unsupported(format!(
        "listing the alternate data streams of {} on Windows",
        path.display()
    )))
}

pub fn xattr_value(path: &Path, name: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut stream = path.as_os_str().to_os_string();
    stream.push(":");
    stream.push(os_from_bytes(name));
    match std::fs::read(&stream) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(super::io(path, error)),
    }
}

pub struct Acl {
    pub kind: &'static str,
    pub tag: String,
    pub qualifier: Value,
    pub perms: String,
}

const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_WRITE_RIGHT: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_READ_DATA: u32 = 0x1;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_EXECUTE: u32 = 0x20;

fn rights(mask: u32) -> String {
    let all = mask & GENERIC_ALL != 0;
    let mut out = String::with_capacity(3);
    out.push(if all || mask & (GENERIC_READ | FILE_READ_DATA) != 0 {
        'r'
    } else {
        '-'
    });
    out.push(
        if all || mask & (GENERIC_WRITE_RIGHT | FILE_WRITE_DATA) != 0 {
            'w'
        } else {
            '-'
        },
    );
    out.push(if all || mask & (GENERIC_EXECUTE | FILE_EXECUTE) != 0 {
        'x'
    } else {
        '-'
    });
    out
}

fn ace_tag(ace_type: AceType) -> &'static str {
    match ace_type {
        AceType::ACCESS_ALLOWED_ACE_TYPE
        | AceType::ACCESS_ALLOWED_CALLBACK_ACE_TYPE
        | AceType::ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
        | AceType::ACCESS_ALLOWED_OBJECT_ACE_TYPE => "allow",
        AceType::ACCESS_DENIED_ACE_TYPE
        | AceType::ACCESS_DENIED_CALLBACK_ACE_TYPE
        | AceType::ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
        | AceType::ACCESS_DENIED_OBJECT_ACE_TYPE => "deny",
        _ => "other",
    }
}

pub fn acl_entries(entry: &crate::row::Entry) -> Result<Vec<Acl>> {
    let path = entry.path();
    let descriptor = security(path, SecurityInformation::Dacl).map_err(|e| super::io(path, e))?;
    let Some(dacl) = descriptor.dacl() else {
        return Ok(Vec::new());
    };
    let accounts = Accounts::default();
    Ok((0..dacl.len())
        .filter_map(|index| dacl.get_ace(index))
        .map(|ace| Acl {
            kind: "access",
            tag: ace_tag(ace.ace_type()).to_owned(),
            qualifier: ace
                .sid()
                .map(|sid| Value::Text(accounts.name(sid)))
                .unwrap_or(Value::Null),
            perms: rights(ace.mask().bits()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wtf8_round_trips_unpaired_surrogates() {
        let original = OsString::from_wide(&[0x61, 0xD800, 0x62, 0xDC00]);
        assert_eq!(os_from_bytes(&os_bytes(&original)), original);
        let text = OsString::from("héllo 🎉");
        assert_eq!(os_from_bytes(&os_bytes(&text)), text);
    }

    #[test]
    fn invalid_bytes_decode_to_replacement_characters() {
        assert_eq!(
            os_from_bytes(b"bytes-\xff"),
            OsString::from("bytes-\u{FFFD}")
        );
    }

    #[test]
    fn sql_paths_take_native_separators() {
        assert_eq!(
            native_path_bytes(br"\\?\C:\tree/.fsql-abc-def-0"),
            PathBuf::from(r"\\?\C:\tree\.fsql-abc-def-0")
        );
    }

    #[test]
    fn relative_paths_render_with_forward_slashes() {
        assert_eq!(slash_bytes(Path::new(r"src\lib.rs")), b"src/lib.rs");
    }

    #[test]
    fn hidden_read_only_files_report_their_attributes_and_owner() {
        let dir = std::env::temp_dir().join(format!("fsql-windows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("tool.exe");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY)
            .open(&file)
            .expect("file");
        let stat = stat_path(&file, false).expect("stat");
        assert!(stat.hidden);
        assert_eq!(stat.mode & 0o777, 0o444);
        assert!(stat.ino != 0);
        assert!(Accounts::default().user(&file, &stat).is_some());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn validated_directories_stay_in_place_while_held() {
        let root = std::env::temp_dir().join(format!("fsql-windows-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("outer").join("inner")).expect("dirs");
        let held = open_chain(&root.join("outer").join("inner")).expect("chain");
        assert!(std::fs::rename(root.join("outer"), root.join("moved")).is_err());
        assert!(std::fs::rename(root.join("outer").join("inner"), root.join("moved")).is_err());
        drop(held);
        std::fs::rename(root.join("outer"), root.join("moved")).expect("rename");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn stored_times_drop_digits_below_a_tick() {
        assert_eq!(
            file_time(1_577_934_245_000_000_123),
            1_577_934_245_000_000_100
        );
        assert_eq!(file_time(-150), -100);
    }

    #[test]
    fn access_masks_render_like_permission_bits() {
        assert_eq!(rights(GENERIC_ALL), "rwx");
        assert_eq!(rights(FILE_READ_DATA | FILE_EXECUTE), "r-x");
        assert_eq!(rights(0), "---");
    }
}
