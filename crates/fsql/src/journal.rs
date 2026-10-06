use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::row::Identity;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<i64>,
    pub mtime: Option<i64>,
    pub path: Option<Vec<u8>>,
    pub target: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Record {
    Delete {
        seq: u64,
        path: Vec<u8>,
        kind: String,
        mode: u32,
        target: Option<Vec<u8>>,
        tomb: Option<Vec<u8>>,
        dev: u64,
        ino: u64,
        ctime: i64,
    },
    Update {
        seq: u64,
        path: Vec<u8>,
        kind: String,
        before: Attrs,
        after: Attrs,
        #[serde(default)]
        identity: Option<Identity>,
    },
    Insert {
        seq: u64,
        path: Vec<u8>,
        kind: String,
        dev: u64,
        ino: u64,
        ctime: i64,
    },
}

impl Record {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Delete { seq, .. } | Self::Update { seq, .. } | Self::Insert { seq, .. } => *seq,
        }
    }
}

pub fn default_base() -> PathBuf {
    if let Some(dir) = std::env::var_os("FSQL_JOURNAL_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data).join("fsql/journal");
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".local/share/fsql/journal"),
        None => PathBuf::from("/var/tmp/fsql/journal"),
    }
}

fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub struct Journal {
    id: String,
    dir: PathBuf,
    seq: u64,
    log: File,
}

impl Journal {
    pub fn open(base: &Path, sql: &str) -> Result<Self> {
        std::fs::create_dir_all(base).map_err(|e| io(base, e))?;
        let nanos = crate::time::now().0;
        let id = format!("{:x}-{:x}", nanos, std::process::id());
        let dir = base.join(&id);
        std::fs::create_dir(&dir).map_err(|e| io(&dir, e))?;
        std::fs::create_dir(dir.join("tomb")).map_err(|e| io(&dir, e))?;
        std::fs::write(dir.join("statement.sql"), sql).map_err(|e| io(&dir, e))?;
        let log_path = dir.join("log.jsonl");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| io(&log_path, e))?;
        File::open(dir.join("statement.sql"))
            .and_then(|f| f.sync_all())
            .map_err(|e| io(&dir, e))?;
        log.sync_all().map_err(|e| io(&log_path, e))?;
        sync_dir(&dir)?;
        sync_dir(base)?;
        Ok(Self {
            id,
            dir,
            seq: 0,
            log,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    pub fn tomb_path(&self, seq: u64) -> PathBuf {
        self.dir.join("tomb").join(seq.to_string())
    }

    /// Persist intent before issuing a syscall. An unfinished intent prevents
    /// automatic undo, preserving the evidence for explicit reconciliation.
    pub fn begin(&mut self, record: &Record) -> Result<()> {
        let path = self.dir.join(format!("pending-{}.json", record.seq()));
        let data = serde_json::to_vec(record).map_err(|e| Error::Plan(e.to_string()))?;
        durable_create(&path, &data)?;
        self.seq = self.seq.max(record.seq() + 1);
        Ok(())
    }

    pub fn cancel(&self, seq: u64) -> Result<()> {
        let path = self.dir.join(format!("pending-{seq}.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => sync_dir(&self.dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(&path, e)),
        }
    }

    pub fn record(&mut self, record: Record) -> Result<()> {
        let line = serde_json::to_string(&record).map_err(|e| Error::Plan(e.to_string()))?;
        let log_path = self.dir.join("log.jsonl");
        self.log
            .write_all(line.as_bytes())
            .and_then(|_| self.log.write_all(b"\n"))
            .and_then(|_| self.log.sync_all())
            .map_err(|e| io(&log_path, e))?;
        self.seq = self.seq.max(record.seq() + 1);
        self.cancel(record.seq())
    }
}

pub struct Summary {
    pub id: String,
    pub statement: String,
    pub records: usize,
    pub recovery_required: bool,
}

pub fn list(base: &Path) -> Result<Vec<Summary>> {
    let mut summaries = Vec::new();
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(summaries),
        Err(e) => return Err(io(base, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io(base, e))?;
        let dir = entry.path();
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let statement = std::fs::read_to_string(dir.join("statement.sql")).unwrap_or_default();
        let loaded = load(base, id);
        let recovery_required = loaded.is_err();
        let records = loaded.map(|r| r.len()).unwrap_or(0);
        summaries.push(Summary {
            id: id.to_owned(),
            statement,
            records,
            recovery_required,
        });
    }
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(summaries)
}

pub fn load(base: &Path, id: &str) -> Result<Vec<Record>> {
    validate_id(id)?;
    let dir = base.join(id);
    for entry in std::fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
        let entry = entry.map_err(|e| io(&dir, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("pending-") || name.starts_with("undo-pending-") {
            return Err(Error::RecoveryRequired(entry.path()));
        }
    }
    let log_path = base.join(id).join("log.jsonl");
    let file = File::open(&log_path).map_err(|e| io(&log_path, e))?;
    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| io(&log_path, e))?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(&line)
            .map_err(|e| Error::Plan(format!("{}: {e}", log_path.display())))?;
        if !dir.join(format!("undone-{}", record.seq())).exists() {
            records.push(record);
        }
    }
    Ok(records)
}

pub fn remove(base: &Path, id: &str) -> Result<()> {
    validate_id(id)?;
    let dir = base.join(id);
    std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
    sync_dir(base)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
        return Err(Error::Plan("invalid journal id".into()));
    }
    Ok(())
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| io(path, e))
}

fn durable_create(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| io(path, e))?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|e| io(path, e))?;
    sync_dir(path.parent().expect("journal parent"))
}

pub(crate) fn begin_undo(base: &Path, id: &str, seq: u64) -> Result<()> {
    durable_create(&base.join(id).join(format!("undo-pending-{seq}")), b"")
}

pub(crate) fn finish_undo(base: &Path, id: &str, seq: u64) -> Result<()> {
    let dir = base.join(id);
    std::fs::rename(
        dir.join(format!("undo-pending-{seq}")),
        dir.join(format!("undone-{seq}")),
    )
    .map_err(|e| io(&dir, e))?;
    sync_dir(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_the_log() {
        let base = std::env::temp_dir().join(format!("fsql-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut journal =
            Journal::open(&base, "delete from files where ext = 'tmp'").expect("open");
        journal
            .record(Record::Delete {
                seq: 0,
                path: b"/x/y".to_vec(),
                kind: "file".to_owned(),
                mode: 0o100644,
                target: None,
                tomb: Some(b"/t/0".to_vec()),
                dev: 1,
                ino: 2,
                ctime: 3,
            })
            .expect("record");
        journal
            .record(Record::Insert {
                seq: 1,
                path: b"/x/z".to_vec(),
                kind: "dir".to_owned(),
                dev: 1,
                ino: 4,
                ctime: 5,
            })
            .expect("record");
        assert_eq!(journal.next_seq(), 2);
        let loaded = load(&base, journal.id()).expect("load");
        assert_eq!(loaded.len(), 2);
        assert!(matches!(&loaded[0], Record::Delete { ino: 2, .. }));
        assert!(matches!(&loaded[1], Record::Insert { ino: 4, .. }));
        let summaries = list(&base).expect("list");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].records, 2);
        assert!(summaries[0].statement.starts_with("delete"));
        remove(&base, journal.id()).expect("remove");
        assert!(list(&base).expect("list").is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
    #[test]
    fn failed_completion_keeps_intent_and_never_reuses_its_sequence() {
        let base =
            std::env::temp_dir().join(format!("fsql-journal-failure-{}", std::process::id()));
        let mut journal = Journal::open(&base, "insert").expect("open");
        let record = Record::Insert {
            seq: 0,
            path: b"/fixture".to_vec(),
            kind: "file".into(),
            dev: 1,
            ino: 2,
            ctime: 3,
        };
        journal.begin(&record).expect("intent");
        journal.log = OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("failure device");
        assert!(journal.record(record).is_err());
        assert_eq!(journal.next_seq(), 1);
        assert!(matches!(
            load(&base, journal.id()),
            Err(Error::RecoveryRequired(_))
        ));
        std::fs::remove_dir_all(base).expect("cleanup");
    }
}
