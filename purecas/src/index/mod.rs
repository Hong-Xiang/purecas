//! Filesystem-first object index: packed layout
//! `.pcas/sha256/<first2>/<hash>--<timestamp>[.<suffix>]`.
//!
//! This module backs `pcas index` and `pcas path`. Neither ever opens or
//! creates `purecas.db`; the packed object entries on disk are the only
//! authoritative state.
//!
//! `pcas index` is a single reconciliation transaction:
//!
//! 1. Refuse a root containing the legacy top-level `purecas.db`.
//! 2. Acquire an exclusive, non-blocking advisory lock on
//!    `.pcas/index.lock` for the whole operation, then clean any stale
//!    `.pcas/tmp` entries left by an interrupted previous run.
//! 3. Scan and validate the complete packed object index (Phase 1).
//!    Malformed names, duplicate digests, conflicting inode claims, and
//!    non-regular entries abort the run before any mutation.
//! 4. Walk pattern-matched visible files, reusing, creating, deduplicating,
//!    or repairing object entries as needed (Phases 2-3). Under
//!    `--rehash`, every retained object is also verified at least once,
//!    even one with no matching visible link this run.
//! 5. Prune every object entry whose fresh hard-link count is one,
//!    independent of the selection pattern (Phase 4).
//!
//! Path-local failures (an unstable file, a failed link, rename, or
//! verification, or a failed prune) accumulate in the report and do not
//! stop independent paths from reconciling; the caller exits non-zero
//! when any occurred.
//!
//! The mtime fast path that skips hashing an already-indexed inode is
//! explicitly non-adversarial: tools that preserve or backdate mtime
//! across a content change (`cp -p`, `rsync -a`, some archive extractors)
//! defeat it silently. Run `pcas index --rehash` after using such a tool.

pub mod discover;
pub mod types;

mod hash;
mod lock;
mod prune;
mod reconcile;
mod scan;

use anyhow::{anyhow, bail, Context, Result};
use discover::{discover_files, Pattern};
use lock::{IndexLock, IndexLockError};
use rustix::fd::OwnedFd;
use rustix::fs::{
    fstat, linkat, mkdirat, openat, openat2, renameat_with, statat, unlinkat, AtFlags, FileType,
    Mode, OFlags, RenameFlags, ResolveFlags,
};
use scan::{scan_object_index, scan_object_index_fd};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use types::{FileSnapshot, FileTypeSuffix, ObjectFileName, RootRelativePath, Sha256Digest};

pub use reconcile::{FailureKind, IndexedFile, PathFailure};

const LEGACY_DATABASE_NAME: &str = "purecas.db";

fn internal_root(root: &Path) -> PathBuf {
    root.join(".pcas")
}

#[cfg(test)]
fn sha256_dir(root: &Path) -> PathBuf {
    scan::sha256_dir(&internal_root(root))
}

fn shard_dir(root: &Path, digest: &Sha256Digest) -> PathBuf {
    scan::shard_dir(&internal_root(root), digest)
}

/// Why resolving a digest against the packed object index failed.
///
/// Every variant carries the human-readable `anyhow::Error` that `pcas
/// path` prints for context; the HTTP digest route instead matches the
/// variant to choose `404` (`NotFound`) or `500` (`CorruptIndex`, `Io`)
/// without ever surfacing the wrapped message (which may contain host
/// paths) to the client.
#[derive(Debug)]
pub enum DigestResolutionError {
    /// The digest string does not parse, or no object entry exists for
    /// it. These are deliberately not distinguished further: neither is
    /// observable by an HTTP client beyond "not found".
    NotFound(anyhow::Error),
    /// The packed index state itself is malformed, unreadable, or
    /// ambiguous (more than one entry for the same digest): store
    /// corruption, not a normal miss.
    CorruptIndex(anyhow::Error),
    /// An unexpected I/O failure unrelated to the index's logical state
    /// (e.g. a transient `read_dir` failure other than "not found").
    Io(anyhow::Error),
}

impl DigestResolutionError {
    /// Recover the wrapped contextual error, e.g. for `pcas path` to print
    /// or propagate via `?`.
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::NotFound(e) | Self::CorruptIndex(e) | Self::Io(e) => e,
        }
    }
}

impl fmt::Display for DigestResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(e) | Self::CorruptIndex(e) | Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DigestResolutionError {}

/// One matching packed object entry found in a digest's shard directory.
struct ShardMatch {
    path: PathBuf,
    suffix: Option<FileTypeSuffix>,
}

/// The outcome of resolving a digest against a single shard directory.
enum ShardLookup {
    NotFound,
    Found(ShardMatch),
    Ambiguous(Vec<PathBuf>),
}

/// Scan only `digest`'s shard directory and find the one object entry
/// whose fixed digest prefix matches. Malformed entries in that shard are
/// reported as store corruption. Used by `pcas path` and the HTTP digest
/// route, which must resolve a single digest without loading the complete
/// object index.
fn scan_shard_for_digest(
    root: &Path,
    digest: &Sha256Digest,
) -> Result<ShardLookup, DigestResolutionError> {
    let dir = shard_dir(root, digest);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ShardLookup::NotFound),
        Err(e) => {
            return Err(DigestResolutionError::Io(
                anyhow::Error::new(e).context(format!("reading shard {}", dir.display())),
            ))
        }
    };

    let mut matches: Vec<ShardMatch> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            DigestResolutionError::Io(
                anyhow::Error::new(e).context(format!("reading shard {}", dir.display())),
            )
        })?;
        let file_name = entry.file_name();
        let file_name = file_name.to_str().ok_or_else(|| {
            DigestResolutionError::CorruptIndex(anyhow!(
                "object entry name is not valid UTF-8: {}",
                entry.path().display()
            ))
        })?;
        let parsed = ObjectFileName::parse(file_name).map_err(|e| {
            DigestResolutionError::CorruptIndex(e.context(format!(
                "malformed object entry {file_name} in {}",
                dir.display()
            )))
        })?;
        if parsed.digest() == digest {
            matches.push(ShardMatch {
                path: entry.path(),
                suffix: parsed.suffix().cloned(),
            });
        }
    }

    match matches.len() {
        0 => Ok(ShardLookup::NotFound),
        1 => Ok(ShardLookup::Found(matches.remove(0))),
        _ => Ok(ShardLookup::Ambiguous(
            matches.into_iter().map(|m| m.path).collect(),
        )),
    }
}

fn ambiguous_error(digest: &Sha256Digest, paths: &[PathBuf]) -> anyhow::Error {
    let listed = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!(
        "ambiguous object entries for digest {digest}: {listed} (store corruption; run `pcas index --rehash`)"
    )
}

/// A single packed object entry resolved for one digest: enough identity
/// to open it once and derive both `pcas path`'s printed path and the HTTP
/// digest route's representation metadata from that same descriptor.
#[derive(Debug)]
pub struct ResolvedDigest {
    pub digest: Sha256Digest,
    pub path: PathBuf,
    pub suffix: Option<FileTypeSuffix>,
}

/// Resolve a raw digest string: parse and normalize it, scan only its
/// shard directory, and return the single matching object entry. Never
/// opens or creates `purecas.db`. Used by both `pcas path` and the HTTP
/// digest route; see [`DigestResolutionError`] for how failures are
/// classified.
pub fn resolve_digest(
    root: &Path,
    raw_digest: &str,
) -> Result<ResolvedDigest, DigestResolutionError> {
    let digest = Sha256Digest::parse(raw_digest).map_err(DigestResolutionError::NotFound)?;
    match scan_shard_for_digest(root, &digest)? {
        ShardLookup::Found(m) => Ok(ResolvedDigest {
            digest,
            path: m.path,
            suffix: m.suffix,
        }),
        ShardLookup::NotFound => Err(DigestResolutionError::NotFound(anyhow!(
            "no indexed object for digest {digest}"
        ))),
        ShardLookup::Ambiguous(paths) => Err(DigestResolutionError::CorruptIndex(ambiguous_error(
            &digest, &paths,
        ))),
    }
}

/// Summary of one `pcas index` run, rendered exactly as
/// `indexed=N reused=N deduplicated=N repaired=N pruned=N failed=N`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IndexSummary {
    /// New object entries created for content with no prior object entry.
    pub indexed: usize,
    /// Visible files whose already-correct digest and inode were reused.
    pub reused: usize,
    /// Visible paths replaced with a hard link to a canonical object.
    pub deduplicated: usize,
    /// Stale object entries renamed or removed because their content
    /// changed since they were indexed.
    pub repaired: usize,
    /// Object entries removed because their fresh hard-link count was one.
    pub pruned: usize,
    /// Independent path-local failures accumulated during the run.
    pub failed: usize,
}

impl fmt::Display for IndexSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "indexed={} reused={} deduplicated={} repaired={} pruned={} failed={}",
            self.indexed, self.reused, self.deduplicated, self.repaired, self.pruned, self.failed
        )
    }
}

/// The full result of one `pcas index` run: every newly created object
/// entry, every independent path failure, and the aggregate summary.
#[derive(Debug, Default, Clone)]
pub struct IndexReport {
    pub created: Vec<IndexedFile>,
    pub failures: Vec<PathFailure>,
    pub summary: IndexSummary,
}

/// Why indexing one exact visible file failed.
#[derive(Debug)]
pub enum IndexFileError {
    /// Another filesystem-first indexing transaction holds the advisory
    /// lock. The visible file has not been changed by this call.
    LockBusy(anyhow::Error),
    /// Validation, packed-index scanning, hashing, or reconciliation failed.
    Failed(anyhow::Error),
}

impl IndexFileError {
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::LockBusy(error) | Self::Failed(error) => error,
        }
    }
}

impl fmt::Display for IndexFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockBusy(error) | Self::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for IndexFileError {}

pub(crate) struct PendingPaths {
    source_parent: OwnedFd,
    source_name: OsString,
    source_lock_name: OsString,
    _source_lock: std::fs::File,
    destination_parent: OwnedFd,
    destination_name: OsString,
}

impl PendingPaths {
    pub(crate) fn new(
        source_parent: OwnedFd,
        source_name: OsString,
        source_lock_name: OsString,
        source_lock: std::fs::File,
        destination_parent: OwnedFd,
        destination_name: OsString,
    ) -> Self {
        Self {
            source_parent,
            source_name,
            source_lock_name,
            _source_lock: source_lock,
            destination_parent,
            destination_name,
        }
    }
}

/// A synced temporary upload that can become visible only after its exact
/// canonical object has been reconciled under the index lock.
pub struct PendingFile {
    root: OwnedFd,
    internal_root: OwnedFd,
    paths: PendingPaths,
    relative_path: RootRelativePath,
    display_root: PathBuf,
    content: PendingContent,
    _file: std::fs::File,
    state: PendingState,
}

#[derive(Clone, Copy)]
enum PendingState {
    Source(FileSnapshot),
    Published(FileSnapshot),
    Quarantined,
    Done,
}

pub(crate) struct PendingContent {
    snapshot: FileSnapshot,
    digest: Sha256Digest,
}

impl PendingContent {
    pub(crate) fn new(snapshot: FileSnapshot, digest: Sha256Digest) -> Self {
        Self { snapshot, digest }
    }
}

impl PendingFile {
    pub(crate) fn new(
        root: OwnedFd,
        internal_root: OwnedFd,
        paths: PendingPaths,
        relative_path: RootRelativePath,
        display_root: PathBuf,
        file: std::fs::File,
        content: PendingContent,
    ) -> Self {
        let snapshot = content.snapshot;
        Self {
            root,
            internal_root,
            paths,
            relative_path,
            display_root,
            content,
            _file: file,
            state: PendingState::Source(snapshot),
        }
    }

    fn entry_snapshot(parent: &impl AsFd, name: &OsStr) -> Result<Option<FileSnapshot>> {
        match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_file() => Ok(Some(FileSnapshot {
                dev: stat.st_dev,
                ino: stat.st_ino,
                size: stat.st_size as u64,
                mtime_sec: stat.st_mtime,
                mtime_nsec: stat.st_mtime_nsec as i64,
            })),
            Ok(_) | Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(descriptor_anyhow(error, "statting quarantined publication")),
        }
    }

    /// Atomically quarantine the destination before deciding whether it is
    /// safe to delete. Returns whether the upload itself was removed.
    fn remove_published(&mut self) -> Result<bool> {
        if let PendingState::Published(expected) = self.state {
            let quarantine_name = rollback_name();
            match renameat_with(
                &self.paths.destination_parent,
                &self.paths.destination_name,
                &self.paths.source_parent,
                &quarantine_name,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => self.state = PendingState::Quarantined,
                Err(rustix::io::Errno::NOENT) => {
                    self.state = PendingState::Done;
                    return Ok(false);
                }
                Err(error) => {
                    self.state = PendingState::Done;
                    return Err(descriptor_anyhow(
                        error,
                        "quarantining visible publication for rollback",
                    ));
                }
            }

            if Self::entry_snapshot(&self.paths.source_parent, &quarantine_name)?
                .is_none_or(|actual| actual.inode_key() != expected.inode_key())
            {
                match renameat_with(
                    &self.paths.source_parent,
                    &quarantine_name,
                    &self.paths.destination_parent,
                    &self.paths.destination_name,
                    RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {}
                    Err(error) => {
                        self.state = PendingState::Done;
                        return Err(descriptor_anyhow(
                            error,
                            "restoring replaced destination after rollback quarantine",
                        ));
                    }
                }
                self.state = PendingState::Done;
                return Ok(false);
            }
            unlinkat(
                &self.paths.source_parent,
                &quarantine_name,
                AtFlags::empty(),
            )
            .map_err(|error| descriptor_anyhow(error, "deleting quarantined publication"))?;
            self.state = PendingState::Done;
        }
        Ok(true)
    }

    fn cleanup_source(&self, expected: FileSnapshot) -> Result<()> {
        let quarantine_name = rollback_name();
        match renameat_with(
            &self.paths.source_parent,
            &self.paths.source_name,
            &self.paths.source_parent,
            &quarantine_name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => {
                return Err(descriptor_anyhow(
                    error,
                    "quarantining private upload link for cleanup",
                ))
            }
        }
        if Self::entry_snapshot(&self.paths.source_parent, &quarantine_name)?
            .is_some_and(|actual| actual.inode_key() == expected.inode_key())
        {
            unlinkat(
                &self.paths.source_parent,
                &quarantine_name,
                AtFlags::empty(),
            )
            .map_err(|error| descriptor_anyhow(error, "deleting quarantined private upload link"))
        } else {
            renameat_with(
                &self.paths.source_parent,
                &quarantine_name,
                &self.paths.source_parent,
                &self.paths.source_name,
                RenameFlags::NOREPLACE,
            )
            .map_err(|error| descriptor_anyhow(error, "restoring replaced private upload link"))
        }
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        match self.state {
            PendingState::Source(expected) => {
                let _ = self.cleanup_source(expected);
            }
            PendingState::Published(_) => {
                let _ = self.remove_published();
            }
            PendingState::Quarantined | PendingState::Done => {}
        }
        let _ = unlinkat(
            &self.paths.source_parent,
            &self.paths.source_lock_name,
            AtFlags::empty(),
        );
    }
}

#[derive(Debug)]
pub enum PendingFileError {
    LockBusy(anyhow::Error),
    Conflict,
    Inaccessible,
    CrossDevice(anyhow::Error),
    Failed(anyhow::Error),
}

/// Remove any `.pcas/tmp` entries left by an interrupted previous run.
/// Called only after the exclusive lock is held, so no concurrent `pcas
/// index` run can be relying on them.
fn clean_stale_tmp(internal_root: &Path) -> Result<()> {
    let tmp_dir = internal_root.join("tmp");
    match fs::remove_dir_all(&tmp_dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cleaning stale {}", tmp_dir.display())),
    }
}

fn reject_legacy_database(root: &Path) -> Result<()> {
    let database = root.join(LEGACY_DATABASE_NAME);
    match fs::symlink_metadata(&database) {
        Ok(_) => bail!(
            "legacy SQLite store detected at {}; migrate it or use a separate root before running `pcas index`",
            database.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("checking legacy database {}", database.display()))
        }
    }
}

/// Run `pcas index [PATTERN] [--rehash]`: the single reconciliation
/// transaction described in the module documentation. Never opens or
/// creates `purecas.db`. Does not print; callers render the report.
pub fn index_root(root: &Path, pattern: Option<&str>, rehash: bool) -> Result<IndexReport> {
    reject_legacy_database(root)?;
    let pattern = Pattern::parse(pattern)?;
    let internal_root = internal_root(root);

    // Hold the lock for the entire scan/reconcile/prune transaction so a
    // concurrent `pcas index` fails clearly instead of racing this one.
    let _lock = IndexLock::acquire(&internal_root).map_err(IndexLockError::into_anyhow)?;
    clean_stale_tmp(&internal_root)?;

    // Phase 1: validate the complete packed object index before any
    // mutation. Corruption here aborts the whole run.
    let mut index = scan_object_index(&internal_root)?;

    // Phases 2-3: reconcile every pattern-matched visible file.
    let mut selected = Vec::new();
    for path in discover_files(root)? {
        let rel = RootRelativePath::from_root(root, &path)?;
        if pattern.matches(&rel) {
            selected.push((rel, path));
        }
    }
    let dirs = reconcile::IndexDirs::for_internal_root(&internal_root);
    let mut outcome = reconcile::reconcile(&dirs, &mut index, &selected, rehash);

    // Phase 4: prune every object entry whose fresh `st_nlink == 1`,
    // independent of the selection pattern.
    let (pruned, prune_failures) = prune::prune(&index);
    outcome.failures.extend(prune_failures);

    let failed = outcome.failures.len();
    Ok(IndexReport {
        created: outcome.created,
        failures: outcome.failures,
        summary: IndexSummary {
            indexed: outcome.indexed,
            reused: outcome.reused,
            deduplicated: outcome.deduplicated,
            repaired: outcome.repaired,
            pruned,
            failed,
        },
    })
}

fn exact_visible_file(root: &Path, relative_path: &RootRelativePath) -> Result<PathBuf> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("canonicalizing root {}", root.display()))?;
    let candidate = canonical_root.join(relative_path.as_path());
    let metadata = fs::symlink_metadata(&candidate)
        .with_context(|| format!("statting exact visible file {}", candidate.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "exact index target is not a non-symlink regular file: {}",
        candidate.display()
    );

    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("canonicalizing exact visible file {}", candidate.display()))?;
    anyhow::ensure!(
        canonical == candidate
            && canonical.starts_with(&canonical_root)
            && !canonical.starts_with(canonical_root.join(".pcas")),
        "exact index target is outside the visible hierarchy: {}",
        candidate.display()
    );
    anyhow::ensure!(
        relative_path.as_path() != Path::new(LEGACY_DATABASE_NAME),
        "the top-level legacy database is not a visible index target"
    );
    Ok(candidate)
}

/// Index exactly one typed root-relative visible file under the same
/// filesystem-first transaction lock as [`index_root`].
///
/// This validates the complete packed object index but never discovers,
/// hashes, reconciles, or prunes any unrelated visible file. It returns the
/// canonical digest/object representation whether the file created, reused,
/// or deduplicated an object entry.
pub fn index_file(
    root: &Path,
    relative_path: &RootRelativePath,
) -> Result<IndexedFile, IndexFileError> {
    reject_legacy_database(root).map_err(IndexFileError::Failed)?;
    let path = exact_visible_file(root, relative_path).map_err(IndexFileError::Failed)?;
    let internal_root = internal_root(root);
    let (mut indexed, _) = index_exact_paths(&internal_root, &path, relative_path, None)?;
    let file_name = indexed
        .object_path
        .file_name()
        .context("indexed object path has no filename")
        .map_err(IndexFileError::Failed)?;
    indexed.object_path = root
        .join(".pcas")
        .join("sha256")
        .join(indexed.digest.shard())
        .join(file_name);
    Ok(indexed)
}

fn index_exact_paths(
    internal_root: &Path,
    path: &Path,
    relative_path: &RootRelativePath,
    expected: Option<FileSnapshot>,
) -> Result<(IndexedFile, FileSnapshot), IndexFileError> {
    let initial_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("statting exact visible file {}", path.display()))
        .map_err(IndexFileError::Failed)?;
    if !initial_metadata.is_file() || initial_metadata.file_type().is_symlink() {
        return Err(IndexFileError::Failed(anyhow!(
            "exact index target is not a non-symlink regular file: {}",
            path.display()
        )));
    }
    if let Some(expected) = expected {
        let initial = FileSnapshot::from_metadata(&initial_metadata);
        if initial != expected {
            return Err(IndexFileError::Failed(anyhow!(
                "published file identity changed before exact indexing"
            )));
        }
    }

    let _lock = match IndexLock::acquire(internal_root) {
        Ok(lock) => lock,
        Err(IndexLockError::Busy(error)) => return Err(IndexFileError::LockBusy(error)),
        Err(IndexLockError::Failed(error)) => return Err(IndexFileError::Failed(error)),
    };

    clean_stale_tmp(internal_root).map_err(IndexFileError::Failed)?;
    let mut index = scan_object_index(internal_root).map_err(IndexFileError::Failed)?;
    let selected = [(relative_path.clone(), path.to_path_buf())];
    let dirs = reconcile::IndexDirs::for_internal_root(internal_root);
    let outcome = reconcile::reconcile(&dirs, &mut index, &selected, false);
    if let Some(failure) = outcome.failures.into_iter().next() {
        return Err(IndexFileError::Failed(anyhow!(failure)));
    }

    // Deduplication replaces the visible inode, so derive the result from a
    // fresh post-reconcile stat rather than from the create-only report.
    let metadata = fs::metadata(path)
        .with_context(|| format!("statting indexed visible file {}", path.display()))
        .map_err(IndexFileError::Failed)?;
    let snapshot = FileSnapshot::from_metadata(&metadata);
    let inode = snapshot.inode_key();
    let digest = index
        .get_digest_at_inode(inode)
        .cloned()
        .with_context(|| {
            format!(
                "indexed inode has no packed object entry: {}",
                path.display()
            )
        })
        .map_err(IndexFileError::Failed)?;
    let object_path = index
        .get_by_digest(&digest)
        .map(|record| record.path.clone())
        .with_context(|| format!("indexed digest has no packed object entry: {digest}"))
        .map_err(IndexFileError::Failed)?;

    Ok((
        IndexedFile {
            digest,
            relative_path: relative_path.clone(),
            object_path,
        },
        snapshot,
    ))
}

fn descriptor_path(fd: &impl AsFd) -> PathBuf {
    PathBuf::from("/proc/self/fd").join(fd.as_fd().as_raw_fd().to_string())
}

fn descriptor_anyhow(error: rustix::io::Errno, context: &'static str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::from_raw_os_error(error.raw_os_error())).context(context)
}

fn open_or_create_internal_dir(parent: &impl AsFd, name: &str) -> Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    loop {
        match openat(parent, name, flags, Mode::empty()) {
            Ok(directory) => return Ok(directory),
            Err(rustix::io::Errno::NOENT) => match mkdirat(parent, name, Mode::from(0o755)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(descriptor_anyhow(error, "creating internal directory")),
            },
            Err(error) => return Err(descriptor_anyhow(error, "opening internal directory")),
        }
    }
}

fn ensure_pending_root_identity(file: &PendingFile) -> Result<()> {
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    let visible_internal = openat2(
        &file.root,
        ".pcas",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )
    .map_err(|error| descriptor_anyhow(error, "opening configured internal index directory"))?;
    let visible_stat = fstat(&visible_internal).map_err(|error| {
        descriptor_anyhow(error, "statting configured internal index directory")
    })?;
    let held_stat = fstat(&file.internal_root)
        .map_err(|error| descriptor_anyhow(error, "statting held internal index directory"))?;
    if visible_stat.st_dev != held_stat.st_dev || visible_stat.st_ino != held_stat.st_ino {
        bail!("top-level .pcas changed during ingestion");
    }

    let displayed =
        fs::metadata(&file.display_root).context("statting configured root pathname")?;
    let displayed = FileSnapshot::from_metadata(&displayed);
    let held_root = fstat(&file.root)
        .map_err(|error| descriptor_anyhow(error, "statting held root directory"))?;
    if displayed.dev != held_root.st_dev || displayed.ino != held_root.st_ino {
        bail!("configured root pathname changed during ingestion");
    }
    Ok(())
}

fn ensure_visible_identity(file: &PendingFile, expected: FileSnapshot) -> Result<()> {
    let opened = openat2(
        &file.root,
        file.relative_path.as_path(),
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|error| descriptor_anyhow(error, "opening published root-relative path"))?;
    let stat =
        fstat(&opened).map_err(|error| descriptor_anyhow(error, "statting published file"))?;
    if stat.st_dev != expected.dev
        || stat.st_ino != expected.ino
        || stat.st_size as u64 != expected.size
        || stat.st_mtime != expected.mtime_sec
        || stat.st_mtime_nsec != expected.mtime_nsec as u64
    {
        bail!("published root-relative path does not identify the indexed upload");
    }
    ensure_pending_root_identity(file)
}

fn same_directory(left: &impl AsFd, right: &impl AsFd) -> Result<bool> {
    let left = fstat(left).map_err(|error| descriptor_anyhow(error, "statting held directory"))?;
    let right =
        fstat(right).map_err(|error| descriptor_anyhow(error, "statting live directory"))?;
    Ok(left.st_dev == right.st_dev && left.st_ino == right.st_ino)
}

fn ensure_object_reachable(
    internal_root: &impl AsFd,
    sha256: &impl AsFd,
    shard: &impl AsFd,
    digest: &Sha256Digest,
    object_name: &OsStr,
    expected: FileSnapshot,
) -> Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let live_sha256 = openat(internal_root, "sha256", flags, Mode::empty())
        .map_err(|error| descriptor_anyhow(error, "opening live sha256 directory"))?;
    if !same_directory(sha256, &live_sha256)? {
        bail!("live sha256 directory changed during ingestion");
    }
    let live_shard = openat(&live_sha256, digest.shard(), flags, Mode::empty())
        .map_err(|error| descriptor_anyhow(error, "opening live digest shard"))?;
    if !same_directory(shard, &live_shard)? {
        bail!("live digest shard changed during ingestion");
    }
    let object = openat(
        &live_shard,
        object_name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| descriptor_anyhow(error, "opening live canonical object"))?;
    let stat = fstat(&object)
        .map_err(|error| descriptor_anyhow(error, "statting live canonical object"))?;
    if stat.st_dev != expected.dev
        || stat.st_ino != expected.ino
        || stat.st_size as u64 != expected.size
        || stat.st_mtime != expected.mtime_sec
        || stat.st_mtime_nsec != expected.mtime_nsec as u64
    {
        bail!("live canonical object does not identify the indexed upload");
    }
    Ok(())
}

struct CreatedEntry {
    name: OsString,
    snapshot: FileSnapshot,
}

struct CreatedObjects {
    entries: Vec<CreatedEntry>,
    parent: std::fs::File,
    quarantine: std::fs::File,
    committed: bool,
}

impl CreatedObjects {
    fn new(parent: &impl AsFd, quarantine: &impl AsFd) -> Result<Self> {
        let parent = std::fs::File::open(descriptor_path(parent))
            .context("opening created-object directory")?;
        let quarantine = std::fs::File::open(descriptor_path(quarantine))
            .context("opening object rollback quarantine")?;
        Ok(Self {
            entries: Vec::new(),
            parent,
            quarantine,
            committed: false,
        })
    }

    fn track(&mut self, objects: &[reconcile::CreatedObject]) {
        self.entries.reserve(objects.len());
        for object in objects {
            let name = object
                .path
                .file_name()
                .expect("created object always has a filename")
                .to_os_string();
            self.entries.push(CreatedEntry {
                name,
                snapshot: object.snapshot,
            });
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

static ROLLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);

fn rollback_name() -> OsString {
    let counter = ROLLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!("rollback-{:x}-{counter:x}", std::process::id()))
}

fn stat_entry(parent: &impl AsFd, name: &OsStr) -> Option<FileSnapshot> {
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    FileType::from_raw_mode(stat.st_mode)
        .is_file()
        .then_some(FileSnapshot {
            dev: stat.st_dev,
            ino: stat.st_ino,
            size: stat.st_size as u64,
            mtime_sec: stat.st_mtime,
            mtime_nsec: stat.st_mtime_nsec as i64,
        })
}

fn rollback_created_entry(parent: &impl AsFd, entry: &CreatedEntry, quarantine: &impl AsFd) {
    let quarantine_name = rollback_name();
    if renameat_with(
        parent,
        &entry.name,
        quarantine,
        &quarantine_name,
        RenameFlags::NOREPLACE,
    )
    .is_err()
    {
        return;
    }
    if stat_entry(quarantine, &quarantine_name)
        .is_some_and(|actual| actual.inode_key() == entry.snapshot.inode_key())
    {
        let _ = unlinkat(quarantine, &quarantine_name, AtFlags::empty());
    } else {
        let _ = renameat_with(
            quarantine,
            &quarantine_name,
            parent,
            &entry.name,
            RenameFlags::NOREPLACE,
        );
    }
}

impl Drop for CreatedObjects {
    fn drop(&mut self) {
        if !self.committed {
            for entry in &self.entries {
                rollback_created_entry(&self.parent, entry, &self.quarantine);
            }
        }
    }
}

fn destination_is_regular(file: &PendingFile) -> Result<bool> {
    match statat(
        &file.paths.destination_parent,
        &file.paths.destination_name,
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) => Ok(FileType::from_raw_mode(stat.st_mode).is_file()),
        Err(error) => Err(descriptor_anyhow(error, "checking raced destination")),
    }
}

/// Reconcile one pre-hashed temporary file and publish it atomically while
/// still holding the global index lock.
pub fn index_and_publish_pending(
    mut file: PendingFile,
    lock_timeout: Duration,
) -> Result<IndexedFile, PendingFileError> {
    ensure_pending_root_identity(&file).map_err(PendingFileError::Failed)?;
    let visible_root = descriptor_path(&file.root);
    reject_legacy_database(&visible_root).map_err(PendingFileError::Failed)?;
    let source = descriptor_path(&file.paths.source_parent).join(&file.paths.source_name);
    let source_snapshot =
        PendingFile::entry_snapshot(&file.paths.source_parent, &file.paths.source_name)
            .map_err(PendingFileError::Failed)?
            .context("synced ingestion temporary file is missing or not regular")
            .map_err(PendingFileError::Failed)?;
    if source_snapshot != file.content.snapshot {
        return Err(PendingFileError::Failed(anyhow!(
            "ingestion temporary file changed after streaming"
        )));
    }

    let _lock = match IndexLock::acquire_at_with_timeout(&file.internal_root, lock_timeout) {
        Ok(lock) => lock,
        Err(IndexLockError::Busy(error)) => return Err(PendingFileError::LockBusy(error)),
        Err(IndexLockError::Failed(error)) => return Err(PendingFileError::Failed(error)),
    };
    ensure_pending_root_identity(&file).map_err(PendingFileError::Failed)?;
    let locked_source =
        PendingFile::entry_snapshot(&file.paths.source_parent, &file.paths.source_name)
            .map_err(PendingFileError::Failed)?
            .context("ingestion temporary file is missing or not regular under index lock")
            .map_err(PendingFileError::Failed)?;
    if locked_source != file.content.snapshot {
        return Err(PendingFileError::Failed(anyhow!(
            "ingestion temporary file changed while waiting for the index lock"
        )));
    }

    let sha256 = open_or_create_internal_dir(&file.internal_root, "sha256")
        .map_err(PendingFileError::Failed)?;
    let tmp = open_or_create_internal_dir(&file.internal_root, "tmp")
        .map_err(PendingFileError::Failed)?;
    let shard = open_or_create_internal_dir(&sha256, file.content.digest.shard())
        .map_err(PendingFileError::Failed)?;
    let dirs = reconcile::IndexDirs::for_pending(
        descriptor_path(&sha256),
        descriptor_path(&tmp),
        file.content.digest.clone(),
        descriptor_path(&shard),
        file._file
            .try_clone()
            .context("cloning held upload descriptor")
            .map_err(PendingFileError::Failed)?,
    );
    let mut index = scan_object_index_fd(&sha256).map_err(PendingFileError::Failed)?;
    let mut created_objects =
        CreatedObjects::new(&shard, &tmp).map_err(PendingFileError::Failed)?;
    let outcome = reconcile::reconcile_prehashed(
        &dirs,
        &mut index,
        &file.relative_path,
        &source,
        &file.content.digest,
        file.content.snapshot,
    );
    created_objects.track(&outcome.created_objects);
    if let Some(failure) = outcome.failures.into_iter().next() {
        return if failure.kind == FailureKind::CrossDevice {
            Err(PendingFileError::CrossDevice(anyhow!(failure)))
        } else {
            Err(PendingFileError::Failed(anyhow!(failure)))
        };
    }
    let validated_snapshot = outcome
        .selected_snapshot
        .context("reconciliation produced no digest-validated snapshot")
        .map_err(PendingFileError::Failed)?;

    let final_source = openat(
        &file.paths.source_parent,
        &file.paths.source_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| descriptor_anyhow(error, "opening reconciled upload inode"))
    .map_err(PendingFileError::Failed)?;
    let final_stat = fstat(&final_source)
        .map_err(|error| descriptor_anyhow(error, "statting reconciled upload inode"))
        .map_err(PendingFileError::Failed)?;
    if !FileType::from_raw_mode(final_stat.st_mode).is_file() {
        return Err(PendingFileError::Failed(anyhow!(
            "reconciled upload is not a regular file"
        )));
    }
    let final_snapshot = FileSnapshot {
        dev: final_stat.st_dev,
        ino: final_stat.st_ino,
        size: final_stat.st_size as u64,
        mtime_sec: final_stat.st_mtime,
        mtime_nsec: final_stat.st_mtime_nsec as i64,
    };
    if final_snapshot != validated_snapshot {
        return Err(PendingFileError::Failed(anyhow!(
            "canonical upload changed after digest verification"
        )));
    }
    file.state = PendingState::Source(final_snapshot);
    let digest = index
        .get_digest_at_inode(final_snapshot.inode_key())
        .cloned()
        .context("reconciled ingestion inode has no packed object entry")
        .map_err(PendingFileError::Failed)?;
    if digest != file.content.digest {
        return Err(PendingFileError::Failed(anyhow!(
            "indexed digest does not match the streamed upload"
        )));
    }
    let record = index
        .get_by_digest(&digest)
        .context("reconciled ingestion digest has no packed object entry")
        .map_err(PendingFileError::Failed)?;
    let object_name = record
        .path
        .file_name()
        .with_context(|| {
            format!(
                "indexed object path has no filename: {}",
                record.path.display()
            )
        })
        .map_err(PendingFileError::Failed)?
        .to_os_string();
    let object_path = file
        .display_root
        .join(".pcas")
        .join("sha256")
        .join(digest.shard())
        .join(&object_name);

    ensure_pending_root_identity(&file).map_err(PendingFileError::Failed)?;
    let held_final_source = descriptor_path(&final_source);
    match linkat(
        rustix::fs::CWD,
        &held_final_source,
        &file.paths.destination_parent,
        &file.paths.destination_name,
        AtFlags::SYMLINK_FOLLOW,
    ) {
        Ok(()) => file.state = PendingState::Published(final_snapshot),
        Err(rustix::io::Errno::EXIST) => {
            return match destination_is_regular(&file) {
                Ok(true) => Err(PendingFileError::Conflict),
                Ok(false) => Err(PendingFileError::Inaccessible),
                Err(error) => Err(PendingFileError::Failed(error)),
            };
        }
        Err(rustix::io::Errno::XDEV) => {
            return Err(PendingFileError::CrossDevice(anyhow!(
                "temporary and destination filesystems differ"
            )));
        }
        Err(error) => {
            return Err(PendingFileError::Failed(descriptor_anyhow(
                error,
                "atomically publishing indexed upload",
            )));
        }
    }
    let commit_check = file
        .cleanup_source(final_snapshot)
        .and_then(|()| ensure_visible_identity(&file, final_snapshot))
        .and_then(|()| {
            ensure_object_reachable(
                &file.internal_root,
                &sha256,
                &shard,
                &digest,
                &object_name,
                final_snapshot,
            )
        });
    if let Err(error) = commit_check {
        match file.remove_published() {
            Ok(true) => {}
            Ok(false) => created_objects.commit(),
            Err(cleanup) => {
                created_objects.commit();
                return Err(PendingFileError::Failed(cleanup.context(format!(
                    "publication identity check also failed: {error:#}"
                ))));
            }
        }
        return Err(PendingFileError::Failed(error));
    }
    created_objects.commit();
    file.state = PendingState::Done;
    Ok(IndexedFile {
        digest,
        relative_path: file.relative_path.clone(),
        object_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &[u8]) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn exact_file_index_does_not_discover_or_prune_unrelated_files() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let selected = write(root, "selected.bin", b"selected");
        let unrelated = write(root, "unrelated.bin", b"unrelated");
        let relative = RootRelativePath::from_relative(Path::new("selected.bin")).unwrap();

        let indexed = index_file(root, &relative).unwrap();

        assert_eq!(indexed.relative_path, relative);
        assert!(indexed.object_path.exists());
        assert_eq!(fs::metadata(selected).unwrap().nlink(), 2);
        assert_eq!(
            fs::metadata(unrelated).unwrap().nlink(),
            1,
            "the unrelated visible file must not be indexed"
        );
        let objects = scan_object_index(&internal_root(root)).unwrap();
        assert_eq!(objects.records().count(), 1);
    }

    #[test]
    fn exact_file_index_returns_existing_canonical_object_after_deduplication() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "first.bin", b"same");
        index_root(root, None, false).unwrap();
        write(root, "second.bin", b"same");
        let relative = RootRelativePath::from_relative(Path::new("second.bin")).unwrap();

        let indexed = index_file(root, &relative).unwrap();

        assert_eq!(
            indexed.digest.as_str(),
            crate::store::hash_file(&root.join("second.bin")).unwrap()
        );
        assert_eq!(
            fs::metadata(root.join("second.bin")).unwrap().ino(),
            fs::metadata(indexed.object_path).unwrap().ino()
        );
    }

    #[test]
    fn patterned_recovery_repairs_corrupt_canonical_before_deduplication() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let original = b"original content";
        write(root, "canonical.bin", original);
        let first = index_root(root, None, false).unwrap();
        let digest = first.created[0].digest.clone();
        fs::write(root.join("canonical.bin"), b"corrupt content!").unwrap();
        write(root, "recovery.bin", original);

        let recovered = index_root(root, Some("recovery.bin"), false).unwrap();

        assert_eq!(recovered.summary.repaired, 1);
        assert_eq!(fs::read(root.join("recovery.bin")).unwrap(), original);
        let object = resolve_digest(root, digest.as_str()).unwrap();
        assert_eq!(fs::read(object.path).unwrap(), original);
    }

    #[test]
    fn patterned_recovery_removes_old_digest_for_mutated_selected_inode() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "canonical.bin", b"digest d");
        write(root, "selected.bin", b"digest e");
        let first = index_root(root, None, false).unwrap();
        let digest_d = first
            .created
            .iter()
            .find(|indexed| indexed.relative_path.as_path() == Path::new("canonical.bin"))
            .unwrap()
            .digest
            .clone();
        let digest_e = first
            .created
            .iter()
            .find(|indexed| indexed.relative_path.as_path() == Path::new("selected.bin"))
            .unwrap()
            .digest
            .clone();
        fs::write(root.join("canonical.bin"), b"corrupt!").unwrap();
        fs::write(root.join("selected.bin"), b"digest d").unwrap();

        let recovered = index_root(root, Some("selected.bin"), true).unwrap();

        assert_eq!(recovered.summary.failed, 0);
        assert_eq!(
            fs::read(resolve_digest(root, digest_d.as_str()).unwrap().path).unwrap(),
            b"digest d"
        );
        assert!(matches!(
            resolve_digest(root, digest_e.as_str()),
            Err(DigestResolutionError::NotFound(_))
        ));
        scan_object_index(&internal_root(root)).unwrap();
    }

    #[test]
    fn exact_file_index_classifies_lock_contention() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "selected.bin", b"selected");
        let relative = RootRelativePath::from_relative(Path::new("selected.bin")).unwrap();
        let _held = IndexLock::acquire(&internal_root(root)).unwrap();

        let error = index_file(root, &relative).unwrap_err();

        assert!(matches!(error, IndexFileError::LockBusy(_)));
    }

    // --- basic create / reuse / idempotence ---------------------------

    #[test]
    fn index_creates_one_hard_linked_entry_with_matching_device_and_inode() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "data/video.mp4", b"hello world");

        let report = index_root(root, None, false).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.summary.reused, 0);

        let object_path = &report.created[0].object_path;
        assert!(object_path.exists());
        assert!(object_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(".mp4"));

        let visible_meta = fs::metadata(&file).unwrap();
        let object_meta = fs::metadata(object_path).unwrap();
        assert_eq!(visible_meta.dev(), object_meta.dev());
        assert_eq!(visible_meta.ino(), object_meta.ino());
        assert_eq!(visible_meta.nlink(), 2);
    }

    #[test]
    fn index_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.txt", b"stable content");

        let first = index_root(root, None, false).unwrap();
        assert_eq!(first.summary.indexed, 1);

        let second = index_root(root, None, false).unwrap();
        assert_eq!(second.summary.indexed, 0);
        assert_eq!(second.summary.reused, 1);

        let shard = shard_dir(
            root,
            &Sha256Digest::parse(&crate::store::hash_file(&root.join("file.txt")).unwrap())
                .unwrap(),
        );
        let entries: Vec<_> = fs::read_dir(&shard).unwrap().collect();
        assert_eq!(entries.len(), 1, "exactly one object entry must exist");
    }

    #[test]
    fn second_unchanged_index_hashes_no_already_indexed_inode() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "stable.bin", b"stable content");

        index_root(root, None, false).unwrap();
        let before = hash::test_support::call_count(&file);
        assert!(before >= 1, "the first run must hash the new file");

        let second = index_root(root, None, false).unwrap();
        assert_eq!(second.summary.reused, 1);
        assert_eq!(second.summary.indexed, 0);

        let after = hash::test_support::call_count(&file);
        assert_eq!(
            after, before,
            "a second unchanged index must not rehash the trusted inode"
        );
    }

    #[test]
    fn index_excludes_dot_pcas() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "visible.txt", b"visible");

        // A pre-existing, well-formed but unreferenced object entry is
        // valid store state (Phase 1 only validates structure, not
        // content), but must never be treated as a discovered visible
        // file; being unreferenced, it is pruned by Phase 4.
        let digest = Sha256Digest::parse(&"c".repeat(64)).unwrap();
        let shard = shard_dir(root, &digest);
        fs::create_dir_all(&shard).unwrap();
        fs::write(
            shard.join(format!("{digest}--20260722T130016Z")),
            b"stray object bytes",
        )
        .unwrap();

        let report = index_root(root, None, false).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.created.len(), 1);
        assert_eq!(
            report.created[0].relative_path.as_path(),
            Path::new("visible.txt")
        );
        assert_eq!(
            report.summary.pruned, 1,
            "the unreferenced stray object entry is pruned, not treated as visible"
        );
    }

    #[test]
    fn index_includes_nested_user_directory_named_dot_pcas() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "visible/.pcas/data.bin", b"nested visible content");

        let report = index_root(root, None, false).unwrap();

        assert_eq!(report.summary.indexed, 1);
        assert_eq!(
            report.created[0].relative_path.as_path(),
            Path::new("visible/.pcas/data.bin")
        );
    }

    #[test]
    fn index_never_follows_symlinks() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let target = write(root, "real.txt", b"real content");
        std::os::unix::fs::symlink(&target, root.join("link.txt")).unwrap();

        let report = index_root(root, None, false).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(
            report.created[0].relative_path.as_path(),
            Path::new("real.txt")
        );
    }

    #[test]
    fn index_basename_pattern_matches_recursively() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "a.mp4", b"a");
        write(root, "nested/b.mp4", b"b");
        write(root, "c.txt", b"c");

        let report = index_root(root, Some("*.mp4"), false).unwrap();
        assert_eq!(report.summary.indexed, 2);
        let names: Vec<_> = report
            .created
            .iter()
            .map(|f| f.relative_path.as_path().to_path_buf())
            .collect();
        assert!(names.contains(&PathBuf::from("a.mp4")));
        assert!(names.contains(&PathBuf::from("nested/b.mp4")));
    }

    #[test]
    fn index_relative_path_pattern_matches_root_relative_paths() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "data/train/a.bin", b"a");
        write(root, "data/test/b.bin", b"b");

        let report = index_root(root, Some("data/train/*.bin"), false).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(
            report.created[0].relative_path.as_path(),
            Path::new("data/train/a.bin")
        );
    }

    #[test]
    fn index_rejects_absolute_pattern() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(index_root(root, Some("/etc/passwd"), false).is_err());
    }

    #[test]
    fn index_rejects_parent_escaping_pattern() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(index_root(root, Some("../escape/*.bin"), false).is_err());
    }

    #[test]
    fn path_resolves_indexed_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"resolve me");
        let report = index_root(root, None, false).unwrap();
        let digest = &report.created[0].digest;

        let resolved = resolve_digest(root, digest.as_str()).unwrap();
        assert_eq!(resolved.path, report.created[0].object_path);
    }

    #[test]
    fn path_fails_for_malformed_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(resolve_digest(root, "not-a-digest").is_err());
    }

    #[test]
    fn path_fails_for_unknown_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let unknown = "0".repeat(64);
        assert!(resolve_digest(root, &unknown).is_err());
    }

    #[test]
    fn path_fails_for_ambiguous_entries() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let digest = Sha256Digest::parse(&"a".repeat(64)).unwrap();
        let shard = shard_dir(root, &digest);
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join(format!("{digest}--20260722T130016Z")), b"x").unwrap();
        fs::write(shard.join(format!("{digest}--20260722T140000Z")), b"x").unwrap();

        let err = resolve_digest(root, digest.as_str()).unwrap_err();
        assert!(matches!(err, DigestResolutionError::CorruptIndex(_)));
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn path_fails_for_malformed_shard_entry() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let digest = Sha256Digest::parse(&"b".repeat(64)).unwrap();
        let shard = shard_dir(root, &digest);
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join("not-a-valid-object-name"), b"x").unwrap();

        assert!(resolve_digest(root, digest.as_str()).is_err());
    }

    #[test]
    fn index_and_path_never_create_purecas_db() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"no sqlite here");

        let report = index_root(root, None, false).unwrap();
        resolve_digest(root, report.created[0].digest.as_str()).unwrap();

        assert!(!root.join("purecas.db").exists());
    }

    #[test]
    fn index_rejects_legacy_database_before_creating_internal_state() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "visible.bin", b"visible");
        fs::write(root.join(LEGACY_DATABASE_NAME), b"legacy sqlite").unwrap();

        let error = index_root(root, None, false).unwrap_err();

        assert!(error.to_string().contains("legacy SQLite store"));
        assert!(!root.join(".pcas").exists());
        assert_eq!(fs::read(root.join("visible.bin")).unwrap(), b"visible");
    }

    #[test]
    fn index_rejects_legacy_database_symlink() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let target = write(root, "database-target", b"legacy sqlite");
        std::os::unix::fs::symlink(target, root.join(LEGACY_DATABASE_NAME)).unwrap();

        let error = index_root(root, None, false).unwrap_err();

        assert!(error.to_string().contains("legacy SQLite store"));
        assert!(!root.join(".pcas").exists());
    }

    #[test]
    fn top_level_sha256_directory_without_database_is_visible_content() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "sha256/data.bin", b"ordinary visible content");

        let report = index_root(root, None, false).unwrap();

        assert_eq!(report.summary.indexed, 1);
        assert_eq!(
            report.created[0].relative_path.as_path(),
            Path::new("sha256/data.bin")
        );
    }

    // --- deduplication of visible files --------------------------------

    #[test]
    fn index_deduplicates_identical_visible_files_onto_same_inode() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let a = write(root, "a.bin", b"same bytes");
        let b = write(root, "b.bin", b"same bytes");

        let report = index_root(root, None, false).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.summary.deduplicated, 1);

        let a_meta = fs::metadata(&a).unwrap();
        let b_meta = fs::metadata(&b).unwrap();
        assert_eq!(a_meta.dev(), b_meta.dev());
        assert_eq!(a_meta.ino(), b_meta.ino());
        assert_eq!(a_meta.nlink(), 3, "a.bin, b.bin, and the object entry");

        let shard = shard_dir(
            root,
            &Sha256Digest::parse(&crate::store::hash_file(&a).unwrap()).unwrap(),
        );
        let entries: Vec<_> = fs::read_dir(&shard).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "identical content creates one object entry"
        );
    }

    #[test]
    fn index_dedup_is_idempotent_on_rerun() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "a.bin", b"same bytes");
        write(root, "b.bin", b"same bytes");
        index_root(root, None, false).unwrap();

        let second = index_root(root, None, false).unwrap();
        assert_eq!(second.summary.reused, 2);
        assert_eq!(second.summary.deduplicated, 0);
        assert_eq!(second.summary.indexed, 0);
    }

    #[test]
    fn index_treats_preexisting_visible_hardlinks_as_same_inode_reuse() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let a = write(root, "a.bin", b"shared");
        let b = root.join("b.bin");
        fs::hard_link(&a, &b).unwrap();

        let report = index_root(root, None, false).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.summary.reused, 1);
        assert_eq!(report.summary.deduplicated, 0);
    }

    // --- repair of in-place mutation ------------------------------------

    #[test]
    fn index_detects_in_place_mutation_via_natural_mtime_bump() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "mutate.bin", b"before");
        let first = index_root(root, None, false).unwrap();
        assert_eq!(first.summary.indexed, 1);

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&file, b"after").unwrap();

        let second = index_root(root, None, false).unwrap();
        assert_eq!(second.summary.repaired, 1);
        assert_eq!(second.summary.indexed, 0);

        let visible_meta = fs::metadata(&file).unwrap();
        let expected_digest = crate::store::hash_file(&file).unwrap();
        let resolved = resolve_digest(root, &expected_digest).unwrap();
        let object_meta = fs::metadata(&resolved.path).unwrap();
        assert_eq!(visible_meta.ino(), object_meta.ino());
    }

    #[test]
    fn rehash_repairs_in_place_mutation_with_preserved_mtime() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "mutate.bin", b"before");

        index_root(root, None, false).unwrap();
        let original_mtime = FileTime::from_last_modification_time(&fs::metadata(&file).unwrap());

        // Simulate a mtime-preserving overwrite tool (e.g. `cp -p`).
        fs::write(&file, b"after-mutation-longer").unwrap();
        filetime::set_file_mtime(&file, original_mtime).unwrap();

        // Without --rehash, the non-adversarial fast path trusts the
        // preserved mtime and misses the change.
        let missed = index_root(root, None, false).unwrap();
        assert_eq!(missed.summary.reused, 1);
        assert_eq!(missed.summary.repaired, 0);

        // `--rehash` always verifies content and repairs it.
        let repaired = index_root(root, None, true).unwrap();
        assert_eq!(repaired.summary.repaired, 1);

        let expected_digest = crate::store::hash_file(&file).unwrap();
        assert!(resolve_digest(root, &expected_digest).is_ok());
    }

    #[test]
    fn rehash_verifies_retained_object_even_when_pattern_excludes_its_visible_link() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "data.bin", b"before");
        index_root(root, None, false).unwrap();

        let original_mtime = FileTime::from_last_modification_time(&fs::metadata(&file).unwrap());
        fs::write(&file, b"after-mutation").unwrap();
        filetime::set_file_mtime(&file, original_mtime).unwrap();

        // The pattern selects nothing, yet --rehash must still verify and
        // repair the retained object.
        let report = index_root(root, Some("*.does-not-match"), true).unwrap();
        assert_eq!(report.summary.repaired, 1);

        let expected_digest = crate::store::hash_file(&file).unwrap();
        assert!(resolve_digest(root, &expected_digest).is_ok());
    }

    #[test]
    fn rehash_removes_pattern_excluded_stale_object_when_digest_is_already_canonical() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let canonical = write(root, "canonical.bin", b"canonical content");
        let changed = write(root, "changed.bin", b"different content");
        index_root(root, None, false).unwrap();

        let stale_digest = crate::store::hash_file(&changed).unwrap();
        let canonical_digest = crate::store::hash_file(&canonical).unwrap();
        let original_mtime =
            FileTime::from_last_modification_time(&fs::metadata(&changed).unwrap());
        fs::write(&changed, b"canonical content").unwrap();
        filetime::set_file_mtime(&changed, original_mtime).unwrap();

        let report = index_root(root, Some("*.does-not-match"), true).unwrap();
        assert_eq!(report.summary.repaired, 1);
        assert_eq!(report.summary.deduplicated, 0);
        assert!(resolve_digest(root, &stale_digest).is_err());
        assert!(resolve_digest(root, &canonical_digest).is_ok());
        assert_ne!(
            fs::metadata(&canonical).unwrap().ino(),
            fs::metadata(&changed).unwrap().ino(),
            "an unselected visible link remains ordinary unindexed content"
        );
    }

    #[test]
    fn rehash_leaves_unchanged_object_name_and_timestamp_intact() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "stable.bin", b"stable content");
        let first = index_root(root, None, false).unwrap();
        let object_path = first.created[0].object_path.clone();

        let report = index_root(root, None, true).unwrap();
        assert_eq!(report.summary.repaired, 0);
        assert_eq!(report.summary.reused, 1);
        assert!(
            object_path.exists(),
            "the object filename (including its index time) must be unchanged"
        );
    }

    #[test]
    fn index_repairs_in_place_mutation_to_content_matching_another_object() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let target = write(root, "target.bin", b"target content");
        let mutated = write(root, "mutate.bin", b"original content");
        index_root(root, None, false).unwrap();

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&mutated, b"target content").unwrap();

        let second = index_root(root, None, false).unwrap();
        assert_eq!(
            second.summary.repaired, 1,
            "the stale object entry for mutate.bin's old digest is removed"
        );
        assert_eq!(
            second.summary.deduplicated, 1,
            "mutate.bin's visible path is replaced onto the canonical inode"
        );

        let target_meta = fs::metadata(&target).unwrap();
        let mutated_meta = fs::metadata(&mutated).unwrap();
        assert_eq!(target_meta.ino(), mutated_meta.ino());
    }

    // --- pruning ---------------------------------------------------------

    #[test]
    fn index_prunes_object_entry_after_visible_deletion() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "gone.bin", b"will be deleted");
        let first = index_root(root, None, false).unwrap();
        let object_path = first.created[0].object_path.clone();
        assert!(object_path.exists());

        fs::remove_file(&file).unwrap();
        let second = index_root(root, None, false).unwrap();
        assert_eq!(second.summary.pruned, 1);
        assert!(!object_path.exists());
    }

    #[test]
    fn index_pattern_limited_run_still_prunes_unrelated_object() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "gone.bin", b"will be deleted");
        write(root, "keep.mp4", b"keep me");
        let first = index_root(root, None, false).unwrap();
        let gone_object = first
            .created
            .iter()
            .find(|c| c.relative_path.as_path() == Path::new("gone.bin"))
            .unwrap()
            .object_path
            .clone();

        fs::remove_file(&file).unwrap();
        let second = index_root(root, Some("*.mp4"), false).unwrap();
        assert_eq!(second.summary.pruned, 1);
        assert!(!gone_object.exists());
    }

    // --- locking and stale tmp cleanup -----------------------------------

    #[test]
    fn index_fails_clearly_while_lock_is_held() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"content");
        let _held = IndexLock::acquire(&internal_root(root)).unwrap();

        let err = index_root(root, None, false).unwrap_err();
        assert!(err.to_string().contains("index.lock"));

        // No mutation must have happened while the lock was held.
        assert!(!sha256_dir(root).exists());
    }

    #[test]
    fn index_cleans_stale_tmp_entries_after_acquiring_lock() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"content");
        let stale_tmp = root.join(".pcas").join("tmp");
        fs::create_dir_all(&stale_tmp).unwrap();
        fs::write(stale_tmp.join("leftover"), b"stale").unwrap();

        index_root(root, None, false).unwrap();
        assert!(!stale_tmp.join("leftover").exists());
    }

    // --- corruption and independent failures ------------------------------

    #[test]
    fn index_root_aborts_before_mutation_on_corrupt_object_index() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"content");
        let shard = sha256_dir(root).join("aa");
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join("not-a-valid-object-name"), b"x").unwrap();

        let err = index_root(root, None, false).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("malformed"));

        let visible_meta = fs::metadata(root.join("file.bin")).unwrap();
        assert_eq!(
            visible_meta.nlink(),
            1,
            "the visible file must remain untouched"
        );
    }

    #[test]
    fn index_accumulates_unreadable_file_failure_while_others_succeed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bad = write(root, "bad.bin", b"unreadable");
        write(root, "good.bin", b"good content");

        let mut perms = fs::metadata(&bad).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&bad, perms).unwrap();

        let result = index_root(root, None, false);

        // Restore permissions unconditionally so `TempDir` cleanup succeeds.
        let mut restore = fs::metadata(&bad).unwrap().permissions();
        restore.set_mode(0o644);
        fs::set_permissions(&bad, restore).unwrap();

        let report = result.unwrap();
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.indexed, 1, "good.bin must still be indexed");
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].path.ends_with("bad.bin"));
    }
}
