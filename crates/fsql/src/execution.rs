//! Per-execution limits and explicit handling of incomplete observations.
use std::cell::Cell;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::{Error, Result, Value};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ErrorPolicy {
    #[default]
    Strict,
    BestEffort,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    scans: Mutex<Vec<tree_fucker::CancellationToken>>,
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<CancellationState>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
        for scan in self
            .0
            .scans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            scan.cancel();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed)
    }

    pub(crate) fn register(&self, scan: tree_fucker::CancellationToken) -> ScanCancellation {
        let mut scans = self.0.scans.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            scan.cancel();
        }
        scans.push(scan.clone());
        ScanCancellation {
            owner: self.clone(),
            scan,
        }
    }
}

pub(crate) struct ScanCancellation {
    owner: CancellationToken,
    scan: tree_fucker::CancellationToken,
}
impl Drop for ScanCancellation {
    fn drop(&mut self) {
        self.owner
            .0
            .scans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|scan| scan != &self.scan);
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionOptions {
    pub error_policy: ErrorPolicy,
    /// Cumulative retained row allocations, including intermediate relations.
    pub max_rows: usize,
    /// Cumulative estimated retained value bytes, including intermediate relations.
    pub max_bytes: usize,
    /// Input rows and candidate join pairs examined across this execution.
    pub max_work: usize,
    pub max_diagnostics: usize,
    pub timeout: Option<Duration>,
    pub cancellation: CancellationToken,
}
impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            error_policy: ErrorPolicy::Strict,
            max_rows: 1_000_000,
            max_bytes: 256 * 1024 * 1024,
            max_work: 10_000_000,
            max_diagnostics: 1_000,
            timeout: None,
            cancellation: CancellationToken::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Completion {
    pub rows: usize,
    pub diagnostics: Vec<Error>,
}
impl Completion {
    pub fn is_complete(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

pub(crate) struct Control {
    pub options: ExecutionOptions,
    started: Instant,
    work: Cell<usize>,
    rows: Cell<usize>,
    bytes: Cell<usize>,
}
impl Control {
    pub fn new(options: ExecutionOptions) -> Self {
        Self {
            options,
            started: Instant::now(),
            work: Cell::new(0),
            rows: Cell::new(0),
            bytes: Cell::new(0),
        }
    }
    fn check(&self) -> Result<()> {
        if self.options.cancellation.is_cancelled() {
            return Err(Error::ResourceLimit("cancelled".into()));
        }
        if self
            .options
            .timeout
            .is_some_and(|limit| self.started.elapsed() >= limit)
        {
            return Err(Error::ResourceLimit("deadline exceeded".into()));
        }
        Ok(())
    }
    pub fn step(&self) -> Result<()> {
        self.check()?;
        let work = self.work.get().saturating_add(1);
        self.work.set(work);
        if work > self.options.max_work {
            return Err(Error::ResourceLimit("work budget exceeded".into()));
        }
        Ok(())
    }
    pub fn retain(&self, bytes: usize) -> Result<()> {
        self.check()?;
        let rows = self.rows.get().saturating_add(1);
        let bytes = self.bytes.get().saturating_add(bytes);
        if rows > self.options.max_rows || bytes > self.options.max_bytes {
            return Err(Error::ResourceLimit(
                "materialization budget exceeded".into(),
            ));
        }
        self.rows.set(rows);
        self.bytes.set(bytes);
        Ok(())
    }
    pub fn allocate(&self, additional: usize) -> Result<()> {
        self.check()?;
        let bytes = self.bytes.get().saturating_add(additional);
        if bytes > self.options.max_bytes {
            return Err(Error::ResourceLimit(
                "materialization byte budget exceeded".into(),
            ));
        }
        self.bytes.set(bytes);
        Ok(())
    }
    pub fn row(&self, values: &[Value]) -> Result<()> {
        self.retain(row_bytes(values))
    }
}

pub(crate) fn row_bytes(values: &[Value]) -> usize {
    values.iter().fold(0usize, |sum, value| {
        sum.saturating_add(std::mem::size_of::<Value>())
            .saturating_add(match value {
                Value::Text(s) => s.len(),
                Value::Blob(b) => b.len(),
                _ => 0,
            })
    })
}
