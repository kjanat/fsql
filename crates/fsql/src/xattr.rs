use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::io::Errno;

use crate::error::{Error, Result};
use crate::eval::{Row, from_bytes};
use crate::exec::{MapRow, RowStream};
use crate::plan::Source;
use crate::row::{Entry, Kind};
use crate::value::Value;
use crate::walk::Walker;

const XATTR_MAX: usize = 65_536;

fn io(path: &Path, errno: Errno) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from(errno),
    }
}

fn absent(errno: Errno) -> bool {
    errno == Errno::NODATA || errno == Errno::OPNOTSUPP || errno == Errno::NOTSUP
}

pub fn names(path: &Path) -> Result<Vec<Vec<u8>>> {
    let mut buffer = vec![0u8; XATTR_MAX];
    let len = match rustix::fs::llistxattr(path, &mut buffer[..]) {
        Ok(len) => len,
        Err(errno) if absent(errno) => return Ok(Vec::new()),
        Err(errno) => return Err(io(path, errno)),
    };
    Ok(buffer[..len]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

pub fn value(path: &Path, name: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut buffer = vec![0u8; XATTR_MAX];
    match rustix::fs::lgetxattr(path, OsStr::from_bytes(name), &mut buffer[..]) {
        Ok(len) => {
            buffer.truncate(len);
            Ok(Some(buffer))
        }
        Err(errno) if absent(errno) => Ok(None),
        Err(errno) => Err(io(path, errno)),
    }
}

fn boxed(map: HashMap<String, Value>) -> Result<Box<dyn Row>> {
    Ok(Box::new(MapRow(map)))
}

fn xattr_rows(entry: &Entry) -> Vec<Result<Box<dyn Row>>> {
    let path = entry.path();
    let names = match names(path) {
        Ok(names) => names,
        Err(error) => return vec![Err(error)],
    };
    names
        .into_iter()
        .map(|name| {
            let bytes = value(path, &name)?.unwrap_or_default();
            let mut map = HashMap::new();
            map.insert("path".to_owned(), from_bytes(path.as_os_str().as_bytes()));
            map.insert("name".to_owned(), from_bytes(&name));
            map.insert(
                "size".to_owned(),
                Value::Int(i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
            );
            map.insert("value".to_owned(), from_bytes(&bytes));
            boxed(map)
        })
        .collect()
}

pub fn rows<'a>(source: &Source) -> Result<RowStream<'a>> {
    let walker = Walker::new(&source.root, source.options.clone())?;
    Ok(Box::new(
        walker
            .flat_map(|entry| match entry {
                Ok(entry) => xattr_rows(&entry),
                Err(error) => vec![Err(error)],
            })
            .map(|row| row.map(|row| row as Box<dyn Row + 'a>)),
    ))
}

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

fn tag_name(tag: u16) -> &'static str {
    match tag {
        ACL_USER_OBJ | ACL_USER => "user",
        ACL_GROUP_OBJ | ACL_GROUP => "group",
        ACL_MASK => "mask",
        ACL_OTHER => "other",
        _ => "unknown",
    }
}

fn acl_rows_for(entry: &Entry) -> Vec<Result<Box<dyn Row>>> {
    let path = entry.path();
    let mut kinds = vec!["access"];
    if entry.kind().ok() == Some(Kind::Dir) {
        kinds.push("default");
    }
    let mut out = Vec::new();
    for kind in kinds {
        let attribute = format!("system.posix_acl_{kind}");
        let bytes = match value(path, attribute.as_bytes()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(error) => {
                out.push(Err(error));
                continue;
            }
        };
        let Some(entries) = parse_acl(&bytes) else {
            continue;
        };
        for acl in entries {
            let mut map = HashMap::new();
            map.insert("path".to_owned(), from_bytes(path.as_os_str().as_bytes()));
            map.insert("kind".to_owned(), Value::Text(kind.to_owned()));
            map.insert("tag".to_owned(), Value::Text(tag_name(acl.tag).to_owned()));
            map.insert("qualifier".to_owned(), qualifier(&acl));
            map.insert("perms".to_owned(), Value::Text(perms(acl.perm)));
            out.push(boxed(map));
        }
    }
    out
}

pub fn acl_rows<'a>(source: &Source) -> Result<RowStream<'a>> {
    let walker = Walker::new(&source.root, source.options.clone())?;
    Ok(Box::new(
        walker
            .flat_map(|entry| match entry {
                Ok(entry) => acl_rows_for(&entry),
                Err(error) => vec![Err(error)],
            })
            .map(|row| row.map(|row| row as Box<dyn Row + 'a>)),
    ))
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

    #[test]
    fn a_plain_temp_file_has_no_extended_attributes_or_errors() {
        let dir = std::env::temp_dir().join(format!("fsql-xattr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("f"), b"x").expect("file");
        let listed = names(&dir.join("f")).expect("names");
        assert!(listed.iter().all(|n| !n.is_empty()));
        assert_eq!(
            value(&dir.join("f"), b"user.fsql.missing").expect("value"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
