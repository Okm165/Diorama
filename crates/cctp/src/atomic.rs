//! Durable file replacement: write a unique temp file in the target
//! directory, `fsync` it, rename it over the target, then `fsync` the
//! directory. A reader therefore sees either the old complete file or the new
//! complete file, never a truncated prefix (the measured failure:
//! `cctp-parked-archive-20261030.json` was 196,608 bytes of an unterminated
//! string after a non-atomic write).
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace `path` atomically with `bytes`, creating its parent directory.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{name}.tmp-{}-{nanos}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        // The bytes must be durable before the rename publishes the name;
        // otherwise a crash can leave the new name pointing at empty blocks.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        // Persist the directory entry too (a rename is metadata).
        sync_dir(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> std::io::Result<()> {
    // Windows cannot open a directory as a file; the temp file's own fsync and
    // the rename remain the atomicity guarantee there.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replacement_leaves_no_temp_file_and_the_complete_bytes() {
        let dir = crate::test_dir("atomic");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
