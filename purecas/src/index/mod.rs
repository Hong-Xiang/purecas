//! Filesystem-first object index: packed layout
//! `.pcas/sha256/<first2>/<hash>--<timestamp>[.<suffix>]`.
//!
//! This module backs `pcas index` and `pcas path`. Neither ever opens or
//! creates `purecas.db`; the packed object entries on disk are the only
//! authoritative state.

pub mod discover;
pub mod types;

use crate::store::hash_file;
use anyhow::{anyhow, bail, Context, Result};
use discover::{discover_files, Pattern};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use types::{FileTypeSuffix, IndexTimestamp, ObjectFileName, RootRelativePath, Sha256Digest};

fn sha256_dir(root: &Path) -> PathBuf {
    root.join(".pcas").join("sha256")
}

fn shard_dir(root: &Path, digest: &Sha256Digest) -> PathBuf {
    sha256_dir(root).join(digest.shard())
}

/// The outcome of resolving a digest against a single shard directory.
enum ShardLookup {
    NotFound,
    Found(PathBuf),
    Ambiguous(Vec<PathBuf>),
}

/// Scan only `digest`'s shard directory and find the one object entry
/// whose fixed digest prefix matches. Malformed entries in that shard are
/// reported as store corruption.
fn scan_shard_for_digest(root: &Path, digest: &Sha256Digest) -> Result<ShardLookup> {
    let dir = shard_dir(root, digest);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ShardLookup::NotFound),
        Err(e) => return Err(e).with_context(|| format!("reading shard {}", dir.display())),
    };

    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading shard {}", dir.display()))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_str().with_context(|| {
            format!(
                "object entry name is not valid UTF-8: {}",
                entry.path().display()
            )
        })?;
        let parsed = ObjectFileName::parse(file_name).with_context(|| {
            format!("malformed object entry {} in {}", file_name, dir.display())
        })?;
        if parsed.digest() == digest {
            matches.push(entry.path());
        }
    }

    match matches.len() {
        0 => Ok(ShardLookup::NotFound),
        1 => Ok(ShardLookup::Found(matches.remove(0))),
        _ => Ok(ShardLookup::Ambiguous(matches)),
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

/// Resolves digests to packed object paths, caching results in an
/// in-memory map so that identical content discovered more than once in a
/// single run is never linked twice.
pub struct DigestResolver {
    cache: HashMap<Sha256Digest, PathBuf>,
}

impl DigestResolver {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Resolve a digest, first from the in-memory map, then by scanning
    /// its shard directory on disk.
    pub fn resolve(&mut self, root: &Path, digest: &Sha256Digest) -> Result<Option<PathBuf>> {
        if let Some(path) = self.cache.get(digest) {
            return Ok(Some(path.clone()));
        }
        match scan_shard_for_digest(root, digest)? {
            ShardLookup::NotFound => Ok(None),
            ShardLookup::Found(path) => {
                self.cache.insert(digest.clone(), path.clone());
                Ok(Some(path))
            }
            ShardLookup::Ambiguous(paths) => Err(ambiguous_error(digest, &paths)),
        }
    }

    /// Record a digest as freshly created, so subsequent lookups in this
    /// run resolve without rescanning the shard.
    fn record(&mut self, digest: Sha256Digest, path: PathBuf) {
        self.cache.insert(digest, path);
    }
}

impl Default for DigestResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Create one hard-linked packed object entry for `digest`, sourced from
/// `source`. The caller must have already confirmed via [`DigestResolver`]
/// that no object entry exists for this digest.
fn create_object_entry(
    root: &Path,
    digest: &Sha256Digest,
    source: &Path,
    resolver: &mut DigestResolver,
) -> Result<PathBuf> {
    let dir = shard_dir(root, digest);
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating shard directory {}", dir.display()))?;

    let suffix = source
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(FileTypeSuffix::infer_from_file_name);
    let name = ObjectFileName::new(digest.clone(), IndexTimestamp::now(), suffix);
    let dest = dir.join(name.to_file_name());

    fs::hard_link(source, &dest).with_context(|| {
        format!(
            "hard-linking {} to object entry {}",
            source.display(),
            dest.display()
        )
    })?;

    resolver.record(digest.clone(), dest.clone());
    Ok(dest)
}

/// A visible file for which a new object entry was created in this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub digest: Sha256Digest,
    pub relative_path: RootRelativePath,
    pub object_path: PathBuf,
}

/// Summary of one `pcas index` run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IndexSummary {
    /// New object entries created in this run.
    pub indexed: usize,
    /// Matched visible files whose digest already had an object entry.
    pub already_indexed: usize,
}

impl fmt::Display for IndexSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "indexed={} already_indexed={}",
            self.indexed, self.already_indexed
        )
    }
}

/// The result of one `pcas index` run: every newly created object entry,
/// in discovery order, plus aggregate counts.
#[derive(Debug, Default, Clone)]
pub struct IndexReport {
    pub created: Vec<IndexedFile>,
    pub summary: IndexSummary,
}

/// Run `pcas index [PATTERN]`: discover matching visible files under
/// `root`, hash each, and create one hard-linked object entry per new
/// digest. Never opens or creates `purecas.db`. Does not print; callers
/// render the report.
pub fn index_root(root: &Path, pattern: Option<&str>) -> Result<IndexReport> {
    let pattern = Pattern::parse(pattern)?;
    let mut resolver = DigestResolver::new();
    let mut report = IndexReport::default();

    for path in discover_files(root)? {
        let rel = RootRelativePath::from_root(root, &path)?;
        if !pattern.matches(&rel) {
            continue;
        }

        let digest = Sha256Digest::parse(&hash_file(&path)?)
            .with_context(|| format!("hashing {}", path.display()))?;

        match resolver.resolve(root, &digest)? {
            Some(_existing) => {
                report.summary.already_indexed += 1;
            }
            None => {
                let object_path = create_object_entry(root, &digest, &path, &mut resolver)?;
                report.created.push(IndexedFile {
                    digest,
                    relative_path: rel,
                    object_path,
                });
                report.summary.indexed += 1;
            }
        }
    }

    Ok(report)
}

/// Run `pcas path <digest>`: parse and normalize the digest, scan only its
/// shard directory, and return the single matching object entry. Never
/// opens or creates `purecas.db`.
pub fn resolve_digest_path(root: &Path, raw_digest: &str) -> Result<PathBuf> {
    let digest = Sha256Digest::parse(raw_digest)?;
    match scan_shard_for_digest(root, &digest)? {
        ShardLookup::Found(path) => Ok(path),
        ShardLookup::NotFound => bail!("no indexed object for digest {digest}"),
        ShardLookup::Ambiguous(paths) => Err(ambiguous_error(&digest, &paths)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
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
    fn index_creates_one_hard_linked_entry_with_matching_device_and_inode() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let file = write(root, "data/video.mp4", b"hello world");

        let report = index_root(root, None).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.summary.already_indexed, 0);

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

        let first = index_root(root, None).unwrap();
        assert_eq!(first.summary.indexed, 1);

        let second = index_root(root, None).unwrap();
        assert_eq!(second.summary.indexed, 0);
        assert_eq!(second.summary.already_indexed, 1);

        let shard = shard_dir(
            root,
            &Sha256Digest::parse(&hash_file(&root.join("file.txt")).unwrap()).unwrap(),
        );
        let entries: Vec<_> = fs::read_dir(&shard).unwrap().collect();
        assert_eq!(entries.len(), 1, "exactly one object entry must exist");
    }

    #[test]
    fn index_deduplicates_identical_content_within_one_run() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "a.bin", b"same bytes");
        write(root, "b.bin", b"same bytes");

        let report = index_root(root, None).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.summary.already_indexed, 1);

        let shard = shard_dir(
            root,
            &Sha256Digest::parse(&hash_file(&root.join("a.bin")).unwrap()).unwrap(),
        );
        let entries: Vec<_> = fs::read_dir(&shard).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "identical content creates one object entry"
        );
    }

    #[test]
    fn index_does_not_deduplicate_visible_files() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let a = write(root, "a.bin", b"same bytes");
        let b = write(root, "b.bin", b"same bytes");

        index_root(root, None).unwrap();

        // Visible files remain independent inodes; only one .pcas entry exists.
        let a_meta = fs::metadata(&a).unwrap();
        let b_meta = fs::metadata(&b).unwrap();
        assert_ne!(a_meta.ino(), b_meta.ino());
    }

    #[test]
    fn index_excludes_dot_pcas() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "visible.txt", b"visible");
        write(root, ".pcas/sha256/aa/stray-file", b"should not be walked");

        let report = index_root(root, None).unwrap();
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(
            report.created[0].relative_path.as_path(),
            Path::new("visible.txt")
        );
    }

    #[test]
    fn index_never_follows_symlinks() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let target = write(root, "real.txt", b"real content");
        std::os::unix::fs::symlink(&target, root.join("link.txt")).unwrap();

        let report = index_root(root, None).unwrap();
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

        let report = index_root(root, Some("*.mp4")).unwrap();
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

        let report = index_root(root, Some("data/train/*.bin")).unwrap();
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
        assert!(index_root(root, Some("/etc/passwd")).is_err());
    }

    #[test]
    fn index_rejects_parent_escaping_pattern() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(index_root(root, Some("../escape/*.bin")).is_err());
    }

    #[test]
    fn path_resolves_indexed_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"resolve me");
        let report = index_root(root, None).unwrap();
        let digest = &report.created[0].digest;

        let resolved = resolve_digest_path(root, digest.as_str()).unwrap();
        assert_eq!(resolved, report.created[0].object_path);
    }

    #[test]
    fn path_fails_for_malformed_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(resolve_digest_path(root, "not-a-digest").is_err());
    }

    #[test]
    fn path_fails_for_unknown_digest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let unknown = "0".repeat(64);
        assert!(resolve_digest_path(root, &unknown).is_err());
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

        let err = resolve_digest_path(root, digest.as_str()).unwrap_err();
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

        assert!(resolve_digest_path(root, digest.as_str()).is_err());
    }

    #[test]
    fn index_and_path_never_create_purecas_db() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(root, "file.bin", b"no sqlite here");

        let report = index_root(root, None).unwrap();
        resolve_digest_path(root, report.created[0].digest.as_str()).unwrap();

        assert!(!root.join("purecas.db").exists());
    }
}
