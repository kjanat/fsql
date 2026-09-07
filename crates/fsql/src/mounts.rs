use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tree_fucker::domain::{
    AccessTopology, DomainCaseSensitivity, DomainProbe, LinuxProbe, MediaHint, TransportHint,
};
use tree_fucker::path::CaseSensitivity;

use crate::error::{Error, Result};
use crate::eval::from_bytes;
use crate::exec::{MapRow, RowStream};
use crate::row::makedev;
use crate::value::Value;

const MOUNTINFO: &str = "/proc/self/mountinfo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub id: u64,
    pub dev: u64,
    pub mountpoint: PathBuf,
    pub fstype: String,
    pub source: String,
    pub options: String,
}

pub fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'\\')
            .then(|| bytes.get(index + 1..index + 4))
            .flatten()
            .and_then(|digits| {
                digits.iter().try_fold(0u16, |acc, d| {
                    let place = d.checked_sub(b'0').filter(|p| *p < 8)?;
                    Some(acc * 8 + u16::from(place))
                })
            })
            .and_then(|value| u8::try_from(value).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 4;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    out
}

pub fn parse_line(line: &str) -> Option<Mount> {
    let mut fields = line.split(' ');
    let id = fields.next()?.parse().ok()?;
    fields.next()?;
    let (major, minor) = fields.next()?.split_once(':')?;
    let dev = makedev(major.parse().ok()?, minor.parse().ok()?);
    fields.next()?;
    let mountpoint = PathBuf::from(OsStr::from_bytes(&unescape(fields.next()?)));
    let options = fields.next()?.to_owned();
    loop {
        if fields.next()? == "-" {
            break;
        }
    }
    let fstype = fields.next()?.to_owned();
    let source = String::from_utf8_lossy(&unescape(fields.next()?)).into_owned();
    Some(Mount {
        id,
        dev,
        mountpoint,
        fstype,
        source,
        options,
    })
}

pub fn read() -> Result<Vec<Mount>> {
    let text = std::fs::read_to_string(MOUNTINFO).map_err(|source| Error::Io {
        path: PathBuf::from(MOUNTINFO),
        source,
    })?;
    Ok(text.lines().filter_map(parse_line).collect())
}

fn lower_debug<T: std::fmt::Debug>(value: T) -> Value {
    let text = format!("{value:?}").to_ascii_lowercase();
    if text == "unknown" {
        Value::Null
    } else {
        Value::Text(text)
    }
}

pub fn row(mount: &Mount, probe: &LinuxProbe) -> MapRow {
    let mut map: HashMap<String, Value> = HashMap::new();
    map.insert(
        "mountpoint".to_owned(),
        from_bytes(mount.mountpoint.as_os_str().as_bytes()),
    );
    map.insert("fstype".to_owned(), Value::Text(mount.fstype.clone()));
    map.insert("source".to_owned(), Value::Text(mount.source.clone()));
    map.insert("options".to_owned(), Value::Text(mount.options.clone()));
    map.insert(
        "readonly".to_owned(),
        Value::Bool(mount.options.split(',').any(|o| o == "ro")),
    );
    map.insert(
        "dev".to_owned(),
        Value::Int(i64::try_from(mount.dev).unwrap_or(i64::MAX)),
    );
    map.insert(
        "mnt_id".to_owned(),
        Value::Int(i64::try_from(mount.id).unwrap_or(i64::MAX)),
    );
    let probed = probe.probe(Path::new(&mount.mountpoint), None).ok();
    let (topology, transport, media, case) = match &probed {
        Some(result) => {
            let caps = &result.capabilities;
            let case = match caps.case {
                DomainCaseSensitivity::Sensitive => Value::Bool(true),
                DomainCaseSensitivity::Insensitive => Value::Bool(false),
                DomainCaseSensitivity::PerDirectory { domain_default } => Value::Bool(
                    result.directory_case.unwrap_or(domain_default) == CaseSensitivity::Sensitive,
                ),
                DomainCaseSensitivity::Unknown => Value::Null,
            };
            (
                lower_debug(caps.topology),
                lower_debug(caps.transport),
                lower_debug(caps.media),
                case,
            )
        }
        None => (Value::Null, Value::Null, Value::Null, Value::Null),
    };
    let remote = match probed.as_ref().map(|r| r.capabilities.topology) {
        Some(AccessTopology::Remote) => Value::Bool(true),
        Some(AccessTopology::Local | AccessTopology::Virtual) => Value::Bool(false),
        Some(AccessTopology::Userspace | AccessTopology::ProtocolNative) => Value::Null,
        Some(AccessTopology::Unknown) | None => Value::Null,
    };
    let _ = (TransportHint::Unknown, MediaHint::Unknown);
    map.insert("topology".to_owned(), topology);
    map.insert("transport".to_owned(), transport);
    map.insert("media".to_owned(), media);
    map.insert("case_sensitive".to_owned(), case);
    map.insert("remote".to_owned(), remote);
    MapRow(map)
}

pub fn rows<'a>() -> Result<RowStream<'a>> {
    let mounts = read()?;
    let probe = LinuxProbe::new();
    let rows: Vec<Result<Box<dyn crate::eval::Row + 'a>>> = mounts
        .iter()
        .map(|mount| Ok(Box::new(row(mount, &probe)) as Box<dyn crate::eval::Row + 'a>))
        .collect();
    Ok(Box::new(rows.into_iter()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Row;

    #[test]
    fn parses_mountinfo_lines_with_and_without_optional_fields() {
        let line =
            "36 35 98:0 /a /mnt/point rw,noatime master:1 - ext4 /dev/root rw,errors=continue";
        let mount = parse_line(line).expect("line");
        assert_eq!(mount.id, 36);
        assert_eq!(mount.dev, makedev(98, 0));
        assert_eq!(mount.mountpoint, PathBuf::from("/mnt/point"));
        assert_eq!(mount.fstype, "ext4");
        assert_eq!(mount.source, "/dev/root");
        assert_eq!(mount.options, "rw,noatime");
        let plain = parse_line("23 28 0:22 / /sys rw,nosuid - sysfs sysfs rw").expect("line");
        assert_eq!(plain.fstype, "sysfs");
        assert_eq!(parse_line("garbage"), None);
    }

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape("/mnt/with\\040space"), b"/mnt/with space");
        assert_eq!(unescape("plain"), b"plain");
        assert_eq!(unescape("bad\\04"), b"bad\\04");
    }

    #[test]
    fn the_root_mount_is_reported_with_its_filesystem() {
        let mounts = read().expect("mounts");
        let root = mounts
            .iter()
            .find(|m| m.mountpoint == Path::new("/"))
            .expect("root mount");
        assert!(!root.fstype.is_empty());
        let row = row(root, &LinuxProbe::new());
        assert_eq!(
            row.column("mountpoint").expect("mountpoint"),
            Value::Text("/".to_owned())
        );
        assert!(matches!(
            row.column("readonly").expect("readonly"),
            Value::Bool(_)
        ));
        assert!(matches!(row.column("dev").expect("dev"), Value::Int(_)));
    }
}
