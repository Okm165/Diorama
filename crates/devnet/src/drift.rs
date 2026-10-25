//! drift.jsonl: the mirror's divergence log — one home for both mirrors.
//!
//! One JSON line per *divergence* (an apply anomaly, a follow anomaly) - not
//! per block. Per-block telemetry lives in the Prometheus metrics; this file
//! is the forensic trail you read when an alarm fires, so it must stay small
//! and readable. Writes are serialized and flushed as a single write per
//! line: the engines' write paths run as separate tasks, and concurrent
//! appends of long lines interleaved and corrupted the file (measured: 10
//! broken lines in 48k).
//!
//! The entry type owns its schema (each producer defines a narrow struct);
//! the log owns the file. A drift write that fails is a typed fault the
//! caller surfaces: a divergence record that could not be written must not
//! look like a clean run.
use crate::error::{Error, Result};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const ROTATE_BYTES: u64 = 16 * 1024 * 1024;

pub struct Drift {
    path: PathBuf,
    /// Serializes whole-line writes. It protects nothing but ordering, so a
    /// poisoned lock is recovered by construction (there is no value a panic
    /// could have left mid-update).
    lock: Mutex<()>,
}

impl Drift {
    /// Create the log's directory. A drift log that cannot exist is a boot
    /// fault, not a silent no-op: it is the forensic trail an alarm points at.
    /// Open the log from an engine section: the state dir's `drift.jsonl` —
    /// the one spelling both profiles use.
    pub fn from_engine(engine: &mirror_config::EngineSection) -> Result<Self> {
        Self::new(engine.state_dir.join("drift.jsonl"))
    }

    pub fn new(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::store::ensure_dir(parent).map_err(|(path, source)| Error::Io {
                what: "drift log directory",
                path,
                source,
            })?;
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    /// One line per entry: the entry type owns its schema (each producer
    /// defines a narrow struct), the log owns the file. A write that fails is
    /// a typed fault the caller surfaces: a divergence record that could not
    /// be written must not look like a clean run.
    pub fn append<T: serde::Serialize>(&self, entry: &T) -> Result<()> {
        let encoded = serde_json::to_string(entry).map_err(|source| Error::Encode {
            what: "drift log entry",
            source,
        })?;
        let line = format!("{encoded}\n");
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.rotate_if_needed()?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| Error::Io {
                what: "drift log",
                path: self.path.clone(),
                source,
            })?;
        f.write_all(line.as_bytes()).map_err(|source| Error::Io {
            what: "drift log",
            path: self.path.clone(),
            source,
        })
    }

    fn rotate_if_needed(&self) -> Result<()> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            // No file yet: the first append creates it.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(Error::Io {
                    what: "drift log",
                    path: self.path.clone(),
                    source,
                })
            }
        };
        if meta.len() < ROTATE_BYTES {
            return Ok(());
        }
        let rotated = self.path.with_extension("jsonl.1");
        std::fs::rename(&self.path, &rotated).map_err(|source| Error::Io {
            what: "drift log rotation",
            path: rotated,
            source,
        })
    }
}
