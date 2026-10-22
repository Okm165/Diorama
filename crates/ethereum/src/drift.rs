//! drift.jsonl: the mirror's divergence log.
//!
//! One JSON line per *divergence* (or apply anomaly) - not per block. Per-block
//! telemetry lives in the Prometheus metrics; this file is the forensic trail
//! you read when an alarm fires, so it must stay small and readable. Writes are
//! serialized and flushed as a single write per line: the M replay and the T
//! applier run as separate tasks, and concurrent appends of long lines
//! interleaved and corrupted the file (measured: 10 broken lines in 48k).
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const ROTATE_BYTES: u64 = 16 * 1024 * 1024;

pub struct Drift {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Drift {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn append(&self, value: &Value) {
        let line = format!("{value}\n");
        let _guard = self.lock.lock().unwrap();
        self.rotate_if_needed();
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let _ = f.write_all(line.as_bytes());
    }

    fn rotate_if_needed(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() < ROTATE_BYTES {
            return;
        }
        let rotated = self.path.with_extension("jsonl.1");
        let _ = std::fs::rename(&self.path, rotated);
    }
}
