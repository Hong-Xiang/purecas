//! Phase 1: read and validate the complete packed object index before any
//! mutation is applied.
//!
//! Every object entry under `.pcas/sha256` is parsed and cross-checked
//! here. Malformed names, duplicate digests, one inode claiming
//! conflicting digests, and non-regular entries are all reconciliation
//! errors that abort the whole run before any mutation is attempted.

use super::types::{FileSnapshot, FileTypeSuffix, IndexTimestamp, ObjectFileName, Sha256Digest};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn sha256_dir(root: &Path) -> PathBuf {
    root.join(".pcas").join("sha256")
}

pub(crate) fn shard_dir(root: &Path, digest: &Sha256Digest) -> PathBuf {
    sha256_dir(root).join(digest.shard())
}

/// One validated object entry as found on disk during Phase 1.
#[derive(Clone, Debug)]
pub(crate) struct ObjectRecord {
    pub digest: Sha256Digest,
    pub timestamp: IndexTimestamp,
    pub suffix: Option<FileTypeSuffix>,
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl ObjectRecord {
    pub(crate) fn inode_key(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

/// The validated in-memory state of `.pcas/sha256`: one map keyed by
/// digest, one keyed by `(device, inode)`, kept consistent with each other
/// as reconciliation mutates them.
#[derive(Debug, Default)]
pub(crate) struct ObjectIndex {
    by_digest: HashMap<Sha256Digest, ObjectRecord>,
    by_inode: HashMap<(u64, u64), Sha256Digest>,
}

impl ObjectIndex {
    pub(crate) fn get_by_digest(&self, digest: &Sha256Digest) -> Option<&ObjectRecord> {
        self.by_digest.get(digest)
    }

    pub(crate) fn get_digest_at_inode(&self, key: (u64, u64)) -> Option<&Sha256Digest> {
        self.by_inode.get(&key)
    }

    /// Insert a newly created or repaired record, keyed consistently by
    /// both digest and inode.
    pub(crate) fn insert(&mut self, record: ObjectRecord) {
        self.by_inode
            .insert(record.inode_key(), record.digest.clone());
        self.by_digest.insert(record.digest.clone(), record);
    }

    /// Remove the record for `digest`, dropping both map entries. Returns
    /// the removed record, if any.
    pub(crate) fn remove(&mut self, digest: &Sha256Digest) -> Option<ObjectRecord> {
        let record = self.by_digest.remove(digest)?;
        self.by_inode.remove(&record.inode_key());
        Some(record)
    }

    /// Every currently retained object record, independent of any
    /// selection pattern used during discovery.
    pub(crate) fn records(&self) -> impl Iterator<Item = &ObjectRecord> {
        self.by_digest.values()
    }
}

/// Scan every shard directory under `.pcas/sha256` and validate every
/// object entry found. Returns before any mutation is applied; malformed
/// names, duplicate digests, conflicting inode claims, and non-regular
/// entries are all reported here as a single corruption error.
pub(crate) fn scan_object_index(root: &Path) -> Result<ObjectIndex> {
    let mut index = ObjectIndex::default();

    let dir = sha256_dir(root);
    let shard_entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(index),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    for shard_entry in shard_entries {
        let shard_entry = shard_entry.with_context(|| format!("reading {}", dir.display()))?;
        let shard_path = shard_entry.path();
        let shard_file_type = shard_entry
            .file_type()
            .with_context(|| format!("reading file type of {}", shard_path.display()))?;
        if !shard_file_type.is_dir() {
            bail!(
                "store corruption: unexpected non-directory entry {} in {}",
                shard_path.display(),
                dir.display()
            );
        }
        let shard_name = shard_entry.file_name();
        let shard_name = shard_name.to_str().with_context(|| {
            format!(
                "shard directory name is not valid UTF-8: {}",
                shard_path.display()
            )
        })?;

        let object_entries = fs::read_dir(&shard_path)
            .with_context(|| format!("reading shard {}", shard_path.display()))?;
        for object_entry in object_entries {
            let object_entry =
                object_entry.with_context(|| format!("reading shard {}", shard_path.display()))?;
            let object_path = object_entry.path();
            let object_file_type = object_entry
                .file_type()
                .with_context(|| format!("reading file type of {}", object_path.display()))?;
            if !object_file_type.is_file() {
                bail!(
                    "store corruption: object entry {} is not a regular file",
                    object_path.display()
                );
            }

            let file_name = object_entry.file_name();
            let file_name = file_name.to_str().with_context(|| {
                format!(
                    "object entry name is not valid UTF-8: {}",
                    object_path.display()
                )
            })?;
            let parsed = ObjectFileName::parse(file_name).with_context(|| {
                format!(
                    "store corruption: malformed object entry {} in {}",
                    file_name,
                    shard_path.display()
                )
            })?;
            if parsed.digest().shard() != shard_name {
                bail!(
                    "store corruption: object entry {} is stored in mismatched shard directory {} (expected {})",
                    file_name,
                    shard_name,
                    parsed.digest().shard()
                );
            }
            if index.by_digest.contains_key(parsed.digest()) {
                bail!(
                    "store corruption: duplicate object entry for digest {}: {}",
                    parsed.digest(),
                    object_path.display()
                );
            }

            let metadata = fs::metadata(&object_path)
                .with_context(|| format!("reading metadata of {}", object_path.display()))?;
            let snapshot = FileSnapshot::from_metadata(&metadata);
            if let Some(existing_digest) = index.by_inode.get(&snapshot.inode_key()) {
                bail!(
                    "store corruption: inode ({}, {}) claims conflicting digests {} and {}",
                    snapshot.dev,
                    snapshot.ino,
                    existing_digest,
                    parsed.digest()
                );
            }

            let record = ObjectRecord {
                digest: parsed.digest().clone(),
                timestamp: parsed.timestamp().clone(),
                suffix: parsed.suffix().cloned(),
                path: object_path,
                dev: snapshot.dev,
                ino: snapshot.ino,
            };
            index.insert(record);
        }
    }

    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn digest(byte: char) -> Sha256Digest {
        Sha256Digest::parse(&byte.to_string().repeat(64)).unwrap()
    }

    fn write_object(
        root: &Path,
        digest: &Sha256Digest,
        timestamp: &str,
        suffix: Option<&str>,
    ) -> PathBuf {
        let shard = shard_dir(root, digest);
        fs::create_dir_all(&shard).unwrap();
        let name = match suffix {
            Some(s) => format!("{digest}--{timestamp}.{s}"),
            None => format!("{digest}--{timestamp}"),
        };
        let path = shard.join(name);
        fs::write(&path, b"object bytes").unwrap();
        path
    }

    #[test]
    fn scan_empty_root_is_empty() {
        let dir = TempDir::new().unwrap();
        let index = scan_object_index(dir.path()).unwrap();
        assert!(index.records().next().is_none());
    }

    #[test]
    fn scan_finds_valid_entries() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let d = digest('a');
        write_object(root, &d, "20260722T130016Z", Some("txt"));

        let index = scan_object_index(root).unwrap();
        let record = index.get_by_digest(&d).unwrap();
        assert_eq!(record.suffix.as_ref().unwrap().as_str(), "txt");
    }

    #[test]
    fn scan_rejects_duplicate_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let d = digest('a');
        write_object(root, &d, "20260722T130016Z", None);
        write_object(root, &d, "20260722T140000Z", None);

        let err = scan_object_index(root).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn scan_rejects_malformed_name() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let shard = sha256_dir(root).join("aa");
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join("not-a-valid-name"), b"x").unwrap();

        let err = scan_object_index(root).unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }

    #[test]
    fn scan_rejects_mismatched_shard() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let d = digest('a');
        let wrong_shard = sha256_dir(root).join("zz");
        fs::create_dir_all(&wrong_shard).unwrap();
        fs::write(wrong_shard.join(format!("{d}--20260722T130016Z")), b"x").unwrap();

        let err = scan_object_index(root).unwrap_err();
        assert!(err.to_string().contains("mismatched shard"));
    }

    #[test]
    fn scan_rejects_conflicting_inode_claim() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let a = digest('a');
        let b = digest('b');
        let path_a = write_object(root, &a, "20260722T130016Z", None);
        let shard_b = shard_dir(root, &b);
        fs::create_dir_all(&shard_b).unwrap();
        let path_b = shard_b.join(format!("{b}--20260722T130016Z"));
        fs::remove_file(&path_b).ok();
        fs::hard_link(&path_a, &path_b).unwrap();

        let err = scan_object_index(root).unwrap_err();
        assert!(err.to_string().contains("conflicting digests"));
    }

    #[test]
    fn scan_rejects_non_regular_entry() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let shard = sha256_dir(root).join("aa");
        fs::create_dir_all(&shard).unwrap();
        let d = digest('a');
        fs::create_dir_all(shard.join(format!("{d}--20260722T130016Z"))).unwrap();

        let err = scan_object_index(root).unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
    }

    #[test]
    fn scan_rejects_non_directory_shard() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(sha256_dir(root)).unwrap();
        fs::write(sha256_dir(root).join("stray-file"), b"x").unwrap();

        let err = scan_object_index(root).unwrap_err();
        assert!(err.to_string().contains("non-directory"));
    }
}
