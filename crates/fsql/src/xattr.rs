use std::collections::HashMap;

use crate::error::Result;
use crate::eval::{Row, from_bytes};
use crate::exec::{MapRow, RowStream};
use crate::plan::Source;
use crate::row::Entry;
use crate::sys;
use crate::value::Value;
use crate::walk::Walker;

pub use sys::{xattr_names as names, xattr_value as value};

#[cfg(unix)]
pub(crate) fn copy(from: &std::path::Path, to: &std::fs::File) -> Result<()> {
    use rustix::io::Errno;
    for name in names(from)? {
        if let Some(value) = value(from, &name)? {
            let name = sys::os_from_bytes(&name);
            match rustix::fs::fsetxattr(to, &name, &value, rustix::fs::XattrFlags::empty()) {
                Ok(()) | Err(Errno::OPNOTSUPP | Errno::PERM | Errno::ACCESS) => {}
                Err(e) => return Err(sys::io(from, e.into())),
            }
        }
    }
    Ok(())
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
            map.insert("path".to_owned(), from_bytes(&sys::path_bytes(path)));
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
    Ok(rows_from(walker))
}

pub(crate) fn rows_from<'a>(walker: Walker) -> RowStream<'a> {
    Box::new(
        walker
            .flat_map(|entry| match entry {
                Ok(entry) => xattr_rows(&entry),
                Err(error) => vec![Err(error)],
            })
            .map(|row| row.map(|row| row as Box<dyn Row + 'a>)),
    )
}

pub use sys::{Acl, acl_entries};

fn acl_rows_for(entry: &Entry) -> Vec<Result<Box<dyn Row>>> {
    let path = entry.path();
    let entries = match acl_entries(entry) {
        Ok(entries) => entries,
        Err(error) => return vec![Err(error)],
    };
    entries
        .into_iter()
        .map(|acl| {
            let mut map = HashMap::new();
            map.insert("path".to_owned(), from_bytes(&sys::path_bytes(path)));
            map.insert("kind".to_owned(), Value::Text(acl.kind.to_owned()));
            map.insert("tag".to_owned(), Value::Text(acl.tag));
            map.insert("qualifier".to_owned(), acl.qualifier);
            map.insert("perms".to_owned(), Value::Text(acl.perms));
            boxed(map)
        })
        .collect()
}

pub fn acl_rows<'a>(source: &Source) -> Result<RowStream<'a>> {
    let walker = Walker::new(&source.root, source.options.clone())?;
    Ok(acls_from(walker))
}

pub(crate) fn acls_from<'a>(walker: Walker) -> RowStream<'a> {
    Box::new(
        walker
            .flat_map(|entry| match entry {
                Ok(entry) => acl_rows_for(&entry),
                Err(error) => vec![Err(error)],
            })
            .map(|row| row.map(|row| row as Box<dyn Row + 'a>)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn listing_alternate_data_streams_is_unsupported_and_reading_one_works() {
        let dir = std::env::temp_dir().join(format!("fsql-streams-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("f");
        std::fs::write(&file, b"x").expect("file");
        let mut stream = file.as_os_str().to_os_string();
        stream.push(":note");
        std::fs::write(&stream, b"hello").expect("stream");
        assert!(matches!(names(&file), Err(crate::Error::Unsupported(_))));
        assert_eq!(
            value(&file, b"note").expect("value"),
            Some(b"hello".to_vec())
        );
        assert_eq!(value(&file, b"missing").expect("value"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
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
