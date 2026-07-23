//! Content hashing with a stability guarantee: a file is only trusted to
//! have hashed to a given digest if its stat-visible identity, size, and
//! mtime are identical immediately before and immediately after the read.

use super::types::{FileSnapshot, Sha256Digest};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Bounded retry count for a file that appears to change while being
/// hashed. Chosen to absorb an occasional concurrent writer without
/// retrying indefinitely against a genuinely unstable file.
const MAX_HASH_ATTEMPTS: u32 = 3;

/// Test-only call-counting instrumentation, keyed by path so concurrent
/// tests never interfere with each other's counts. This lets tests prove
/// that a trusted inode was *not* rehashed without relying on timing.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static CALLS: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);

    pub(crate) fn record_call(path: &Path) {
        let mut guard = CALLS.lock().unwrap();
        *guard
            .get_or_insert_with(HashMap::new)
            .entry(path.to_path_buf())
            .or_insert(0) += 1;
    }

    pub(crate) fn call_count(path: &Path) -> usize {
        let guard = CALLS.lock().unwrap();
        guard
            .as_ref()
            .and_then(|calls| calls.get(path))
            .copied()
            .unwrap_or(0)
    }
}

/// A digest computed from a file whose stat snapshot was confirmed
/// unchanged across the read.
pub(crate) struct StableHash {
    pub digest: Sha256Digest,
    pub snapshot: FileSnapshot,
}

/// Hash `path`, using the same open file descriptor to snapshot stat state
/// before and after reading. If the snapshots differ, retry up to
/// [`MAX_HASH_ATTEMPTS`] times; a file that keeps changing becomes an
/// explicit path failure rather than a silently wrong digest.
pub(crate) fn hash_file_stable(path: &Path) -> Result<StableHash> {
    #[cfg(test)]
    test_support::record_call(path);

    for _attempt in 0..MAX_HASH_ATTEMPTS {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let pre = FileSnapshot::from_metadata(
            &file
                .metadata()
                .with_context(|| format!("reading metadata of {}", path.display()))?,
        );
        let digest = hash_open_file(&file, path)?;
        let post = FileSnapshot::from_metadata(
            &file
                .metadata()
                .with_context(|| format!("reading metadata of {}", path.display()))?,
        );
        if pre == post {
            return Ok(StableHash {
                digest: Sha256Digest::parse(&digest)
                    .with_context(|| format!("hashing {}", path.display()))?,
                snapshot: post,
            });
        }
    }
    bail!(
        "{} changed while being hashed ({MAX_HASH_ATTEMPTS} attempts); it is not a stable regular file",
        path.display()
    )
}

fn hash_open_file(file: &File, path: &Path) -> Result<String> {
    let mut reader = file;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn hash_stable_file_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.bin");
        fs::write(&path, b"hello world").unwrap();

        let result = hash_file_stable(&path).unwrap();
        assert_eq!(
            result.digest.as_str(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn hash_missing_file_fails() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.bin");
        assert!(hash_file_stable(&path).is_err());
    }

    #[test]
    fn hash_continuously_mutated_file_fails_after_bounded_retries() {
        use std::io::{Seek, SeekFrom, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("flapping.bin");
        // Large enough that hashing (I/O plus SHA-256 computation) takes
        // a little real time, giving a genuinely concurrent writer many
        // chances to land a write mid-read; a tiny file's read completes
        // from the page cache too fast for any writer cadence to reliably
        // interleave.
        fs::write(&path, vec![0u8; 16 * 1024 * 1024]).unwrap();

        // Rewrite one byte in place (no truncation, no size change) so
        // the file is never observed at a transient, coincidentally
        // stable size; only its mtime keeps advancing. A fixed iteration
        // count (no sleep, no stop flag) bounds the writer's own runtime
        // so this test can never hang, regardless of how the race
        // resolves, while writing as fast as possible maximizes the
        // chance of landing inside the reader's narrow window.
        let mut writer_file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let writer_started = Arc::clone(&started);
        let writer = thread::spawn(move || {
            for i in 0..2_000_000u32 {
                let _ = writer_file.seek(SeekFrom::Start(0));
                let _ = writer_file.write_all(&[(i % 256) as u8]);
                writer_started.store(true, Ordering::Release);
            }
        });

        // Do not start reading until the writer is already hammering
        // writes: OS thread-spawn latency alone can otherwise exceed the
        // time it takes to hash a cached, warm file, letting the reader
        // "win" every attempt purely by finishing before the writer's
        // first scheduled quantum.
        while !started.load(Ordering::Acquire) {
            thread::yield_now();
        }

        let result = hash_file_stable(&path);
        writer.join().unwrap();

        assert!(
            result.is_err(),
            "a continuously mutated file must be reported as unstable, not silently hashed"
        );
    }
}
