//! Advisory exclusive locking for `.pcas/index.lock`.
//!
//! `pcas index` holds this lock for its entire scan/reconcile/prune
//! transaction so that a second concurrent index process fails clearly
//! instead of racing on-disk state. The lock only coordinates cooperating
//! `pcas` processes on the local machine; it cannot prevent arbitrary
//! external filesystem mutation.
//!
//! This uses `std::fs::File`'s own advisory file-locking API (stable
//! since Rust 1.89) rather than a third-party crate or hand-rolled
//! `flock`/`LockFileEx` bindings: it is the most minimal, portable, and
//! maintained implementation available.

use anyhow::{bail, Context, Result};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

fn lock_path(root: &Path) -> PathBuf {
    root.join(".pcas").join("index.lock")
}

/// A held exclusive lock on `.pcas/index.lock`. Dropping it releases the
/// lock (an OS-level consequence of closing the underlying file handle).
#[derive(Debug)]
pub(crate) struct IndexLock {
    _file: File,
}

impl IndexLock {
    /// Acquire the lock without blocking. Fails clearly, before any
    /// scanning or mutation, if another `pcas index` run already holds it.
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        let dot_pcas = root.join(".pcas");
        fs::create_dir_all(&dot_pcas)
            .with_context(|| format!("creating {}", dot_pcas.display()))?;

        let path = lock_path(root);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => bail!(
                "another `pcas index` run holds {} (advisory lock); wait for it to finish",
                path.display()
            ),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_creates_dot_pcas_and_lock_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let _lock = IndexLock::acquire(root).unwrap();
        assert!(lock_path(root).exists());
    }

    #[test]
    fn acquire_fails_while_already_held() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let _held = IndexLock::acquire(root).unwrap();

        let err = IndexLock::acquire(root).unwrap_err();
        assert!(err.to_string().contains("index.lock"));
    }

    #[test]
    fn acquire_succeeds_again_after_lock_is_dropped() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        {
            let _held = IndexLock::acquire(root).unwrap();
        }
        assert!(IndexLock::acquire(root).is_ok());
    }
}
