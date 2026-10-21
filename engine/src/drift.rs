//! drift.jsonl: one JSON line per processed block - the mirror's flight
//! recorder. Rotated at 256 MiB.
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

const ROTATE_BYTES: u64 = 256 * 1024 * 1024;

pub struct Drift {
    path: PathBuf,
}

impl Drift {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Self { path }
    }

    pub fn append(&self, value: &Value) {
        self.rotate_if_needed();
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let _ = writeln!(f, "{value}");
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
