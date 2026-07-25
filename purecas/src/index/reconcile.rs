//! Phases 2-3: walk pattern-matched visible files, reconcile each against
//! the validated object index, and (under `--rehash`) verify every
//! retained object at least once even when the pattern excludes all of
//! its visible links.
//!
//! See the repair rules in issue #6 / the parent design (issue #2) for the
//! exact case analysis implemented here.

use super::hash::hash_file_stable;
use super::scan::{ObjectIndex, ObjectRecord};
use super::types::{
    FileSnapshot, FileTypeSuffix, IndexTimestamp, ObjectFileName, RootRelativePath, Sha256Digest,
};
use anyhow::{Context, Result};
use rustix::fs::{linkat, renameat_with, statat, unlinkat, AtFlags, FileType, RenameFlags, CWD};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The category of an independent path-local failure. Corruption of the
/// packed object index itself (Phase 1) is never represented here: it
/// aborts the whole run before any of these can occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The file's stat snapshot kept changing while it was hashed.
    Unstable,
    /// A hard link or rename step crossed a filesystem boundary.
    CrossDevice,
    /// Creating a hard link to or from an object entry failed.
    LinkFailed,
    /// Renaming an object entry, or a temporary link over a visible path,
    /// failed for a reason other than crossing devices.
    RenameFailed,
    /// A replacement path's inode did not match the canonical object
    /// after an otherwise successful rename.
    VerifyFailed,
    /// Re-statting or unlinking an object entry during pruning failed.
    PruneFailed,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Unstable => "unstable",
            Self::CrossDevice => "cross-device",
            Self::LinkFailed => "link-failed",
            Self::RenameFailed => "rename-failed",
            Self::VerifyFailed => "verify-failed",
            Self::PruneFailed => "prune-failed",
        };
        f.write_str(s)
    }
}

/// One independent path-local failure. Failures accumulate in the report;
/// they never abort processing of other paths.
#[derive(Debug, Clone)]
pub struct PathFailure {
    pub path: PathBuf,
    pub kind: FailureKind,
    pub message: String,
}

impl fmt::Display for PathFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}]: {}",
            self.path.display(),
            self.kind,
            self.message
        )
    }
}

/// A visible file for which a new object entry was created in this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub digest: Sha256Digest,
    pub relative_path: RootRelativePath,
    pub object_path: PathBuf,
}

/// Aggregate outcome of Phases 2, 3, and (under `--rehash`) the retained
/// object verification pass.
#[derive(Debug, Default)]
pub struct ReconcileOutcome {
    pub created: Vec<IndexedFile>,
    pub(crate) created_objects: Vec<CreatedObject>,
    pub failures: Vec<PathFailure>,
    pub indexed: usize,
    pub reused: usize,
    pub deduplicated: usize,
    pub repaired: usize,
    pub(crate) selected_snapshot: Option<FileSnapshot>,
}

#[derive(Debug)]
pub(crate) struct CreatedObject {
    pub path: PathBuf,
    pub snapshot: FileSnapshot,
}

struct ReconcileState<'a> {
    index: &'a mut ObjectIndex,
    verified: &'a mut HashSet<(u64, u64)>,
    outcome: &'a mut ReconcileOutcome,
}

pub(crate) struct IndexDirs {
    sha256_root: PathBuf,
    tmp_root: PathBuf,
    selected_shard: Option<(Sha256Digest, PathBuf)>,
    selected_source: Option<fs::File>,
}

impl IndexDirs {
    pub(crate) fn for_internal_root(internal_root: &Path) -> Self {
        Self {
            sha256_root: internal_root.join("sha256"),
            tmp_root: internal_root.join("tmp"),
            selected_shard: None,
            selected_source: None,
        }
    }

    pub(crate) fn for_pending(
        sha256_root: PathBuf,
        tmp_root: PathBuf,
        digest: Sha256Digest,
        shard: PathBuf,
        source: fs::File,
    ) -> Self {
        Self {
            sha256_root,
            tmp_root,
            selected_shard: Some((digest, shard)),
            selected_source: Some(source),
        }
    }

    fn shard(&self, digest: &Sha256Digest) -> PathBuf {
        self.selected_shard
            .as_ref()
            .filter(|(selected, _)| selected == digest)
            .map(|(_, path)| path.clone())
            .unwrap_or_else(|| self.sha256_root.join(digest.shard()))
    }
}

fn suffix_from_path(path: &Path) -> Option<FileTypeSuffix> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(FileTypeSuffix::infer_from_file_name)
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A path under `internal_root/tmp` guaranteed unique within this process, used to
/// stage a hard link before it is atomically renamed over a visible path.
/// Staging here keeps an interrupted replacement out of visible discovery
/// so a later `pcas index` can clean it up safely.
fn unique_tmp_path(dirs: &IndexDirs) -> Result<PathBuf> {
    let tmp_dir = &dirs.tmp_root;
    fs::create_dir_all(tmp_dir).with_context(|| format!("creating {}", tmp_dir.display()))?;
    let pid = std::process::id();
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(tmp_dir.join(format!("{pid:x}-{counter:x}-{nanos:x}")))
}

/// Create one hard-linked object entry for `digest`, sourced from
/// `source`. The caller must already have confirmed no object entry
/// exists for this digest.
fn create_object_entry(
    dirs: &IndexDirs,
    digest: &Sha256Digest,
    suffix: Option<FileTypeSuffix>,
    source: &Path,
    expected_snapshot: FileSnapshot,
) -> Result<ObjectRecord> {
    let dir = dirs.shard(digest);
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating shard directory {}", dir.display()))?;
    let name = ObjectFileName::new(digest.clone(), IndexTimestamp::now(), suffix);
    let dest = dir.join(name.to_file_name());
    if let Some(source_fd) = &dirs.selected_source {
        let destination_dir =
            fs::File::open(&dir).with_context(|| format!("opening {}", dir.display()))?;
        let held_source = PathBuf::from("/proc/self/fd").join(source_fd.as_raw_fd().to_string());
        linkat(
            CWD,
            &held_source,
            &destination_dir,
            name.to_file_name(),
            AtFlags::SYMLINK_FOLLOW,
        )
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
        .with_context(|| {
            format!(
                "hard-linking held upload inode to object entry {}",
                dest.display()
            )
        })?;
    } else {
        fs::hard_link(source, &dest).with_context(|| {
            format!(
                "hard-linking {} to object entry {}",
                source.display(),
                dest.display()
            )
        })?;
    }
    let metadata =
        fs::metadata(&dest).with_context(|| format!("reading metadata of {}", dest.display()))?;
    let snapshot = FileSnapshot::from_metadata(&metadata);
    if snapshot != expected_snapshot {
        quarantine_mismatched_object(dirs, &dest, expected_snapshot)?;
        anyhow::bail!(
            "{} was replaced between hashing and object publication",
            source.display()
        );
    }
    Ok(ObjectRecord {
        digest: digest.clone(),
        timestamp: name.timestamp().clone(),
        suffix: name.suffix().cloned(),
        path: dest,
        snapshot,
    })
}

fn quarantine_mismatched_object(
    dirs: &IndexDirs,
    object: &Path,
    expected: FileSnapshot,
) -> Result<()> {
    let parent_path = object
        .parent()
        .context("mismatched object has no parent directory")?;
    let name = object
        .file_name()
        .context("mismatched object has no filename")?;
    fs::create_dir_all(&dirs.tmp_root)
        .with_context(|| format!("creating {}", dirs.tmp_root.display()))?;
    let parent = fs::File::open(parent_path)
        .with_context(|| format!("opening {}", parent_path.display()))?;
    let quarantine = fs::File::open(&dirs.tmp_root)
        .with_context(|| format!("opening {}", dirs.tmp_root.display()))?;
    let quarantine_name = format!(
        "mismatch-{:x}-{:x}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    match renameat_with(
        &parent,
        name,
        &quarantine,
        &quarantine_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(std::io::Error::from_raw_os_error(error.raw_os_error()))
                .context("quarantining mismatched object entry")
        }
    }
    let stat = statat(&quarantine, &quarantine_name, AtFlags::SYMLINK_NOFOLLOW).ok();
    let matches = stat.is_some_and(|stat| {
        FileType::from_raw_mode(stat.st_mode).is_file()
            && stat.st_dev == expected.dev
            && stat.st_ino == expected.ino
    });
    if matches {
        unlinkat(&quarantine, &quarantine_name, AtFlags::empty())
            .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
            .context("deleting verified mismatched object entry")?;
    }
    Ok(())
}

fn link_failure_kind(error: &anyhow::Error) -> FailureKind {
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::CrossesDevices)
    {
        FailureKind::CrossDevice
    } else {
        FailureKind::LinkFailed
    }
}

/// Rename a stale object entry to a newly packed name for `new_digest`,
/// updating both maps. Used both when a selected visible path exposes an
/// in-place content change (suffix re-derived from that path) and when
/// `--rehash` repairs a retained object with no matching visible link
/// this run (suffix preserved from the stale entry).
fn rename_object_in_place(
    dirs: &IndexDirs,
    index: &mut ObjectIndex,
    old_digest: &Sha256Digest,
    new_digest: Sha256Digest,
    suffix: Option<FileTypeSuffix>,
) -> Result<()> {
    let old_record = index
        .remove(old_digest)
        .context("stale object record vanished during repair")?;
    let new_dir = dirs.shard(&new_digest);
    fs::create_dir_all(&new_dir)
        .with_context(|| format!("creating shard directory {}", new_dir.display()))?;
    let name = ObjectFileName::new(new_digest.clone(), IndexTimestamp::now(), suffix);
    let new_path = new_dir.join(name.to_file_name());
    fs::rename(&old_record.path, &new_path).with_context(|| {
        format!(
            "renaming stale object entry {} to {}",
            old_record.path.display(),
            new_path.display()
        )
    })?;
    let metadata = fs::metadata(&new_path)
        .with_context(|| format!("reading metadata of {}", new_path.display()))?;
    let snapshot = FileSnapshot::from_metadata(&metadata);
    index.insert(ObjectRecord {
        digest: new_digest,
        timestamp: name.timestamp().clone(),
        suffix: name.suffix().cloned(),
        path: new_path,
        snapshot,
    });
    Ok(())
}

/// Remove a stale object entry whose digest is now known to be wrong,
/// leaving any of its remaining visible hard links as ordinary,
/// unindexed content.
fn remove_stale_object_entry(index: &mut ObjectIndex, old_digest: &Sha256Digest) -> Result<()> {
    let record = index
        .remove(old_digest)
        .context("stale object record vanished during repair")?;
    fs::remove_file(&record.path)
        .with_context(|| format!("removing stale object entry {}", record.path.display()))
}

/// Deduplicate `visible_path` onto `canonical_object_path`: stage a hard
/// link under `.pcas/tmp`, atomically rename it over the visible path,
/// and verify the replacement now shares the canonical inode. Cleans the
/// temporary link on every error and never falls back to copying.
fn dedup_visible_onto_canonical(
    dirs: &IndexDirs,
    canonical_object_path: &Path,
    visible_path: &Path,
) -> Result<(), PathFailure> {
    let fail = |kind: FailureKind, message: String| PathFailure {
        path: visible_path.to_path_buf(),
        kind,
        message,
    };

    let tmp_path = unique_tmp_path(dirs).map_err(|e| {
        fail(
            FailureKind::LinkFailed,
            format!("staging temporary link: {e}"),
        )
    })?;

    fs::hard_link(canonical_object_path, &tmp_path).map_err(|e| {
        let kind = if e.kind() == std::io::ErrorKind::CrossesDevices {
            FailureKind::CrossDevice
        } else {
            FailureKind::LinkFailed
        };
        fail(
            kind,
            format!(
                "hard-linking canonical object {} to temporary {}: {e}",
                canonical_object_path.display(),
                tmp_path.display()
            ),
        )
    })?;

    if let Err(e) = fs::rename(&tmp_path, visible_path) {
        let _ = fs::remove_file(&tmp_path);
        let kind = if e.kind() == std::io::ErrorKind::CrossesDevices {
            FailureKind::CrossDevice
        } else {
            FailureKind::RenameFailed
        };
        return Err(fail(
            kind,
            format!(
                "renaming temporary link over {}: {e}",
                visible_path.display()
            ),
        ));
    }

    let canonical_meta = fs::metadata(canonical_object_path).map_err(|e| {
        fail(
            FailureKind::VerifyFailed,
            format!("re-reading canonical object metadata: {e}"),
        )
    })?;
    let replaced_meta = fs::metadata(visible_path).map_err(|e| {
        fail(
            FailureKind::VerifyFailed,
            format!("re-reading replaced path metadata: {e}"),
        )
    })?;
    let canonical_snapshot = FileSnapshot::from_metadata(&canonical_meta);
    let replaced_snapshot = FileSnapshot::from_metadata(&replaced_meta);
    if canonical_snapshot.inode_key() != replaced_snapshot.inode_key() {
        return Err(fail(
            FailureKind::VerifyFailed,
            "replaced path does not reference the canonical object inode after rename".to_string(),
        ));
    }
    Ok(())
}

/// Reconcile one already-computed digest for a selected visible path
/// against the object index, applying the create/reuse/dedup/repair rules
/// from the design.
fn apply_digest_for_visible_path(
    dirs: &IndexDirs,
    rel: &RootRelativePath,
    path: &Path,
    digest: &Sha256Digest,
    snapshot: FileSnapshot,
    state: &mut ReconcileState<'_>,
) -> Result<(), PathFailure> {
    let inode_key = snapshot.inode_key();
    let existing_for_digest = state.index.get_by_digest(digest).cloned();
    if let Some(record) = &existing_for_digest {
        if record.inode_key() == inode_key {
            state.outcome.reused += 1;
            state.outcome.selected_snapshot = Some(snapshot);
            return Ok(());
        }
    }
    let existing_for_inode = state.index.get_digest_at_inode(inode_key).cloned();
    let mut verified_canonical = None;

    if let Some(record) = &existing_for_digest {
        let canonical = hash_file_stable(&record.path).map_err(|error| PathFailure {
            path: record.path.clone(),
            kind: FailureKind::Unstable,
            message: error.to_string(),
        })?;
        if canonical.digest != *digest {
            remove_stale_object_entry(state.index, digest).map_err(|error| PathFailure {
                path: record.path.clone(),
                kind: FailureKind::RenameFailed,
                message: error.to_string(),
            })?;
            if let Some(old_digest) = &existing_for_inode {
                rename_object_in_place(
                    dirs,
                    state.index,
                    old_digest,
                    digest.clone(),
                    suffix_from_path(rel.as_path()),
                )
                .map_err(|error| PathFailure {
                    path: path.to_path_buf(),
                    kind: FailureKind::RenameFailed,
                    message: error.to_string(),
                })?;
            } else {
                let replacement = create_object_entry(
                    dirs,
                    digest,
                    suffix_from_path(rel.as_path()),
                    path,
                    snapshot,
                )
                .map_err(|error| PathFailure {
                    path: path.to_path_buf(),
                    kind: link_failure_kind(&error),
                    message: error.to_string(),
                })?;
                state.outcome.created.push(IndexedFile {
                    digest: digest.clone(),
                    relative_path: rel.clone(),
                    object_path: replacement.path.clone(),
                });
                state.outcome.created_objects.push(CreatedObject {
                    path: replacement.path.clone(),
                    snapshot: replacement.snapshot,
                });
                state.index.insert(replacement);
            }
            state.outcome.repaired += 1;
            state.outcome.selected_snapshot = Some(snapshot);
            return Ok(());
        }
        verified_canonical = Some(canonical.snapshot);
    }

    match existing_for_digest {
        None => match existing_for_inode {
            None => {
                let record = create_object_entry(
                    dirs,
                    digest,
                    suffix_from_path(rel.as_path()),
                    path,
                    snapshot,
                )
                .map_err(|e| PathFailure {
                    path: path.to_path_buf(),
                    kind: link_failure_kind(&e),
                    message: e.to_string(),
                })?;
                state.outcome.created.push(IndexedFile {
                    digest: digest.clone(),
                    relative_path: rel.clone(),
                    object_path: record.path.clone(),
                });
                state.outcome.created_objects.push(CreatedObject {
                    path: record.path.clone(),
                    snapshot: record.snapshot,
                });
                state.index.insert(record);
                state.outcome.indexed += 1;
                state.outcome.selected_snapshot = Some(snapshot);
            }
            Some(old_digest) => {
                rename_object_in_place(
                    dirs,
                    state.index,
                    &old_digest,
                    digest.clone(),
                    suffix_from_path(rel.as_path()),
                )
                .map_err(|e| PathFailure {
                    path: path.to_path_buf(),
                    kind: FailureKind::RenameFailed,
                    message: e.to_string(),
                })?;
                state.outcome.repaired += 1;
                state.outcome.selected_snapshot = Some(snapshot);
            }
        },
        Some(record) => {
            if let Some(old_digest) = existing_for_inode {
                remove_stale_object_entry(state.index, &old_digest).map_err(|e| PathFailure {
                    path: path.to_path_buf(),
                    kind: FailureKind::RenameFailed,
                    message: e.to_string(),
                })?;
                state.outcome.repaired += 1;
            }
            dedup_visible_onto_canonical(dirs, &record.path, path)?;
            state.outcome.deduplicated += 1;
            state.outcome.selected_snapshot = verified_canonical;
        }
    }
    Ok(())
}

/// Reconcile one selected visible file: trust its indexed digest without
/// hashing when possible, otherwise hash it stably and reconcile the
/// result.
fn reconcile_one_visible_file(
    dirs: &IndexDirs,
    rel: &RootRelativePath,
    path: &Path,
    rehash: bool,
    state: &mut ReconcileState<'_>,
) -> Result<(), PathFailure> {
    let metadata = fs::metadata(path).map_err(|e| PathFailure {
        path: path.to_path_buf(),
        kind: FailureKind::Unstable,
        message: format!("statting {}: {e}", path.display()),
    })?;
    let snapshot = FileSnapshot::from_metadata(&metadata);

    if !rehash {
        if let Some(digest) = state
            .index
            .get_digest_at_inode(snapshot.inode_key())
            .cloned()
        {
            let record = state
                .index
                .get_by_digest(&digest)
                .expect("inode map stays consistent with digest map");
            if snapshot.mtime_at_or_before(&record.timestamp) {
                state.outcome.reused += 1;
                state.verified.insert(snapshot.inode_key());
                return Ok(());
            }
        }
    }

    let hashed = hash_file_stable(path).map_err(|e| PathFailure {
        path: path.to_path_buf(),
        kind: FailureKind::Unstable,
        message: e.to_string(),
    })?;
    state.verified.insert(hashed.snapshot.inode_key());

    apply_digest_for_visible_path(dirs, rel, path, &hashed.digest, hashed.snapshot, state)
}

/// Reconcile one retained object with no visible link verified this run
/// (`--rehash` only): repair it in place if its bytes changed, otherwise
/// leave it untouched. Returns whether a repair occurred.
fn reconcile_retained_object(
    dirs: &IndexDirs,
    index: &mut ObjectIndex,
    old_digest: &Sha256Digest,
    object_path: &Path,
    new_digest: Sha256Digest,
) -> Result<bool, PathFailure> {
    if &new_digest == old_digest {
        return Ok(false);
    }

    let existing = index.get_by_digest(&new_digest).cloned();
    match existing {
        None => {
            let suffix = index
                .get_by_digest(old_digest)
                .and_then(|r| r.suffix.clone());
            rename_object_in_place(dirs, index, old_digest, new_digest, suffix).map_err(|e| {
                PathFailure {
                    path: object_path.to_path_buf(),
                    kind: FailureKind::RenameFailed,
                    message: e.to_string(),
                }
            })?;
        }
        Some(_canonical) => {
            remove_stale_object_entry(index, old_digest).map_err(|e| PathFailure {
                path: object_path.to_path_buf(),
                kind: FailureKind::RenameFailed,
                message: e.to_string(),
            })?;
        }
    }
    Ok(true)
}

/// Under `--rehash`, verify every retained object inode not already
/// verified through a selected visible link this run, repairing mismatches.
fn reconcile_retained_objects(
    dirs: &IndexDirs,
    index: &mut ObjectIndex,
    verified: &HashSet<(u64, u64)>,
    outcome: &mut ReconcileOutcome,
) {
    let candidates: Vec<(Sha256Digest, PathBuf)> = index
        .records()
        .filter(|record| !verified.contains(&record.inode_key()))
        .map(|record| (record.digest.clone(), record.path.clone()))
        .collect();

    for (old_digest, object_path) in candidates {
        // A prior candidate in this same pass may already have repaired
        // this exact entry away (e.g. two stale entries resolving to the
        // same canonical digest in sequence); skip if so.
        if index.get_by_digest(&old_digest).is_none() {
            continue;
        }
        match hash_file_stable(&object_path) {
            Err(e) => outcome.failures.push(PathFailure {
                path: object_path,
                kind: FailureKind::Unstable,
                message: e.to_string(),
            }),
            Ok(hashed) => {
                match reconcile_retained_object(
                    dirs,
                    index,
                    &old_digest,
                    &object_path,
                    hashed.digest,
                ) {
                    Ok(true) => outcome.repaired += 1,
                    Ok(false) => {}
                    Err(failure) => outcome.failures.push(failure),
                }
            }
        }
    }
}

/// Run Phases 2-3 (and, under `--rehash`, retained-object verification)
/// against `index`, mutating it in place to reflect every applied repair.
pub(crate) fn reconcile(
    dirs: &IndexDirs,
    index: &mut ObjectIndex,
    selected: &[(RootRelativePath, PathBuf)],
    rehash: bool,
) -> ReconcileOutcome {
    let mut outcome = ReconcileOutcome::default();
    let mut verified = HashSet::new();

    {
        let mut state = ReconcileState {
            index,
            verified: &mut verified,
            outcome: &mut outcome,
        };
        for (rel, path) in selected {
            if let Err(failure) = reconcile_one_visible_file(dirs, rel, path, rehash, &mut state) {
                state.outcome.failures.push(failure);
            }
        }
    }

    if rehash {
        reconcile_retained_objects(dirs, index, &verified, &mut outcome);
    }

    outcome
}

/// Reconcile one already-hashed file without reading its bytes again. Used by
/// HTTP ingestion after the request stream has been hashed and synced outside
/// the global index lock.
pub(crate) fn reconcile_prehashed(
    dirs: &IndexDirs,
    index: &mut ObjectIndex,
    relative_path: &RootRelativePath,
    path: &Path,
    digest: &Sha256Digest,
    snapshot: FileSnapshot,
) -> ReconcileOutcome {
    let mut outcome = ReconcileOutcome::default();
    let mut verified = HashSet::new();
    let mut state = ReconcileState {
        index,
        verified: &mut verified,
        outcome: &mut outcome,
    };
    if let Err(failure) =
        apply_digest_for_visible_path(dirs, relative_path, path, digest, snapshot, &mut state)
    {
        state.outcome.failures.push(failure);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn internal(root: &Path) -> PathBuf {
        root.join(".pcas")
    }

    fn dirs(root: &Path) -> IndexDirs {
        IndexDirs::for_internal_root(&internal(root))
    }

    #[test]
    fn dedup_onto_canonical_reports_link_failed_when_canonical_source_is_missing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let visible = root.join("visible.bin");
        fs::write(&visible, b"x").unwrap();
        let missing_canonical = root.join("does-not-exist");

        let err =
            dedup_visible_onto_canonical(&dirs(root), &missing_canonical, &visible).unwrap_err();
        assert_eq!(err.kind, FailureKind::LinkFailed);
    }

    #[test]
    fn dedup_onto_canonical_cleans_temp_and_reports_rename_failed_on_missing_parent() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let canonical = root.join("canonical.bin");
        fs::write(&canonical, b"canonical bytes").unwrap();
        // The visible path's parent directory does not exist, so the
        // final rename must fail.
        let visible = root.join("missing-parent-dir").join("visible.bin");

        let err = dedup_visible_onto_canonical(&dirs(root), &canonical, &visible).unwrap_err();
        assert_eq!(err.kind, FailureKind::RenameFailed);

        let tmp_dir = root.join(".pcas").join("tmp");
        let remaining: Vec<_> = fs::read_dir(&tmp_dir).unwrap().collect();
        assert!(
            remaining.is_empty(),
            "the staged temporary link must be cleaned up after a failed rename"
        );
    }

    #[test]
    fn dedup_onto_canonical_succeeds_and_shares_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let canonical = root.join("canonical.bin");
        fs::write(&canonical, b"canonical bytes").unwrap();
        let visible = root.join("visible.bin");
        fs::write(&visible, b"different bytes for now").unwrap();

        dedup_visible_onto_canonical(&dirs(root), &canonical, &visible).unwrap();

        let canonical_meta = fs::metadata(&canonical).unwrap();
        let visible_meta = fs::metadata(&visible).unwrap();
        assert_eq!(canonical_meta.dev(), visible_meta.dev());
        assert_eq!(canonical_meta.ino(), visible_meta.ino());

        let tmp_dir = root.join(".pcas").join("tmp");
        let remaining: Vec<_> = fs::read_dir(&tmp_dir).unwrap().collect();
        assert!(
            remaining.is_empty(),
            "the temporary link must not remain after success"
        );
    }

    #[test]
    fn object_creation_rejects_source_replaced_after_hashing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let source = root.join("source.bin");
        fs::write(&source, b"original").unwrap();
        let _original = fs::File::open(&source).unwrap();
        let expected = FileSnapshot::from_metadata(&fs::metadata(&source).unwrap());
        fs::remove_file(&source).unwrap();
        fs::write(&source, b"replacement").unwrap();
        let digest = Sha256Digest::parse(&"a".repeat(64)).unwrap();

        let error = create_object_entry(&dirs(root), &digest, None, &source, expected).unwrap_err();

        assert!(error.to_string().contains("replaced between hashing"));
        assert_eq!(
            fs::read_dir(dirs(root).shard(&digest)).unwrap().count(),
            0,
            "the mismatched object entry must be removed"
        );
    }
}
