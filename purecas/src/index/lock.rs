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

use anyhow::{anyhow, Context};
use rustix::fs::{fstat, open, openat, FileType, Mode, OFlags};
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::os::fd::AsFd;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(test)]
fn lock_path(internal_root: &Path) -> PathBuf {
    internal_root.join("index.lock")
}

/// A held exclusive lock on `.pcas/index.lock`. Dropping it releases the
/// lock (an OS-level consequence of closing the underlying file handle).
#[derive(Debug)]
pub(crate) struct IndexLock {
    _file: File,
}

#[derive(Debug)]
pub(crate) enum IndexLockError {
    Busy(anyhow::Error),
    Failed(anyhow::Error),
}

impl IndexLockError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Busy(error) | Self::Failed(error) => error,
        }
    }
}

impl fmt::Display for IndexLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(error) | Self::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for IndexLockError {}

impl IndexLock {
    /// Acquire the lock without blocking. Fails clearly, before any
    /// scanning or mutation, if another `pcas index` run already holds it.
    pub(crate) fn acquire(internal_root: &Path) -> Result<Self, IndexLockError> {
        Self::acquire_with_timeout(internal_root, Duration::ZERO)
    }

    /// Wait for at most `timeout`, retrying a contended advisory lock without
    /// blocking any async runtime worker.
    pub(crate) fn acquire_with_timeout(
        internal_root: &Path,
        timeout: Duration,
    ) -> Result<Self, IndexLockError> {
        fs::create_dir_all(internal_root)
            .with_context(|| format!("creating {}", internal_root.display()))
            .map_err(IndexLockError::Failed)?;
        let directory = open(
            internal_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
        .with_context(|| format!("opening {}", internal_root.display()))
        .map_err(IndexLockError::Failed)?;
        Self::acquire_at_with_timeout(&directory, timeout)
    }

    pub(crate) fn acquire_at_with_timeout(
        internal_root: &impl AsFd,
        timeout: Duration,
    ) -> Result<Self, IndexLockError> {
        let started = Instant::now();
        let lock = openat(
            internal_root,
            "index.lock",
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from(0o600),
        )
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
        .context("opening descriptor-relative index.lock")
        .map_err(IndexLockError::Failed)?;
        let stat = fstat(&lock)
            .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
            .context("statting descriptor-relative index.lock")
            .map_err(IndexLockError::Failed)?;
        if !FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(IndexLockError::Failed(anyhow!(
                "descriptor-relative index.lock is not a regular file"
            )));
        }
        let file = File::from(lock);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(IndexLockError::Busy(anyhow!(
                        "another `pcas index` run holds {} (advisory lock); waited {} ms",
                        ".pcas/index.lock",
                        started.elapsed().as_millis()
                    )))
                }
                Err(TryLockError::Error(e)) => {
                    return Err(IndexLockError::Failed(
                        anyhow::Error::new(e).context("locking descriptor-relative index.lock"),
                    ))
                }
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
        let internal_root = dir.path().join(".pcas");
        let _lock = IndexLock::acquire(&internal_root).unwrap();
        assert!(lock_path(&internal_root).exists());
    }

    #[test]
    fn acquire_fails_while_already_held() {
        let dir = TempDir::new().unwrap();
        let internal_root = dir.path().join(".pcas");
        let _held = IndexLock::acquire(&internal_root).unwrap();

        let err = IndexLock::acquire(&internal_root).unwrap_err();
        assert!(err.to_string().contains("index.lock"));
    }

    #[test]
    fn acquire_succeeds_again_after_lock_is_dropped() {
        let dir = TempDir::new().unwrap();
        let internal_root = dir.path().join(".pcas");
        {
            let _held = IndexLock::acquire(&internal_root).unwrap();
        }
        assert!(IndexLock::acquire(&internal_root).is_ok());
    }

    #[test]
    fn acquire_rejects_symlinked_lock_file_without_touching_target() {
        let dir = TempDir::new().unwrap();
        let internal_root = dir.path().join(".pcas");
        fs::create_dir(&internal_root).unwrap();
        let target = dir.path().join("outside-lock");
        std::os::unix::fs::symlink(&target, lock_path(&internal_root)).unwrap();

        let error = IndexLock::acquire(&internal_root).unwrap_err();

        assert!(matches!(error, IndexLockError::Failed(_)));
        assert!(!target.exists());
    }

    #[test]
    fn acquire_rejects_non_regular_lock_file() {
        let dir = TempDir::new().unwrap();
        let internal_root = dir.path().join(".pcas");
        fs::create_dir(&internal_root).unwrap();
        fs::create_dir(lock_path(&internal_root)).unwrap();

        assert!(matches!(
            IndexLock::acquire(&internal_root),
            Err(IndexLockError::Failed(_))
        ));
    }
}
