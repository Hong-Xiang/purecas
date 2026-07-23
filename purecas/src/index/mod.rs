//! Filesystem-first object index: packed layout
//! `.pcas/sha256/<first2>/<hash>--<timestamp>[.<suffix>]`.
//!
//! This module backs `pcas index` and `pcas path`. Neither ever opens or
//! creates `purecas.db`; the packed object entries on disk are the only
//! authoritative state.
//!
//! `pcas index` is a single reconciliation transaction:
//!
//! 1. Acquire an exclusive, non-blocking advisory lock on
//!    `.pcas/index.lock` for the whole operation, then clean any stale
//!    `.pcas/tmp` entries left by an interrupted previous run.
//! 2. Scan and validate the complete packed object index (Phase 1).
//!    Malformed names, duplicate digests, conflicting inode claims, and
//!    non-regular entries abort the run before any mutation.
//! 3. Walk pattern-matched visible files, reusing, creating, deduplicating,
//!    or repairing object entries as needed (Phases 2-3). Under
//!    `--rehash`, every retained object is also verified at least once,
//!    even one with no matching visible link this run.
//! 4. Prune every object entry whose fresh hard-link count is one,
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

use anyhow::{anyhow, Context, Result};
use discover::{discover_files, Pattern};
use lock::IndexLock;
use scan::scan_object_index;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use types::{FileTypeSuffix, ObjectFileName, RootRelativePath, Sha256Digest};

pub use reconcile::{FailureKind, IndexedFile, PathFailure};

fn sha256_dir(root: &Path) -> PathBuf {
    root.join(".pcas").join("sha256")
}

fn shard_dir(root: &Path, digest: &Sha256Digest) -> PathBuf {
    sha256_dir(root).join(digest.shard())
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

/// Remove any `.pcas/tmp` entries left by an interrupted previous run.
/// Called only after the exclusive lock is held, so no concurrent `pcas
/// index` run can be relying on them.
fn clean_stale_tmp(root: &Path) -> Result<()> {
    let tmp_dir = root.join(".pcas").join("tmp");
    match fs::remove_dir_all(&tmp_dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cleaning stale {}", tmp_dir.display())),
    }
}

/// Run `pcas index [PATTERN] [--rehash]`: the single reconciliation
/// transaction described in the module documentation. Never opens or
/// creates `purecas.db`. Does not print; callers render the report.
pub fn index_root(root: &Path, pattern: Option<&str>, rehash: bool) -> Result<IndexReport> {
    let pattern = Pattern::parse(pattern)?;

    // Hold the lock for the entire scan/reconcile/prune transaction so a
    // concurrent `pcas index` fails clearly instead of racing this one.
    let _lock = IndexLock::acquire(root)?;
    clean_stale_tmp(root)?;

    // Phase 1: validate the complete packed object index before any
    // mutation. Corruption here aborts the whole run.
    let mut index = scan_object_index(root)?;

    // Phases 2-3: reconcile every pattern-matched visible file.
    let mut selected = Vec::new();
    for path in discover_files(root)? {
        let rel = RootRelativePath::from_root(root, &path)?;
        if pattern.matches(&rel) {
            selected.push((rel, path));
        }
    }
    let mut outcome = reconcile::reconcile(root, &mut index, &selected, rehash);

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
        let _held = IndexLock::acquire(root).unwrap();

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
