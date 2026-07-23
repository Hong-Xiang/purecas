//! Canonical root/`.pcas` boundaries and safe per-request path resolution.
//!
//! Every visible target is canonicalized and checked for containment
//! *before* it is opened. A regular file is then opened exactly once, and
//! all downstream representation metadata and body bytes come from that
//! same descriptor — never from a second, path-based lookup — so a
//! concurrent atomic replacement of the visible path cannot make the ETag
//! describe different bytes than the streamed body. This is Unix-only, like
//! the rest of the filesystem-first object layout.

use anyhow::{Context, Result};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The canonicalized `PCAS_ROOT` and (if it exists) its internal `.pcas`
/// directory, established once at server startup.
pub struct Root {
    canonical_root: PathBuf,
    canonical_pcas: Option<PathBuf>,
}

impl Root {
    /// Canonicalize `configured` and fail clearly if it does not exist or
    /// is not a directory. Synchronous: called once at startup, before the
    /// async runtime is handling any request.
    pub fn open(configured: &Path) -> Result<Self> {
        let canonical_root = std::fs::canonicalize(configured)
            .with_context(|| format!("PCAS_ROOT {} does not exist", configured.display()))?;
        let meta = std::fs::metadata(&canonical_root)
            .with_context(|| format!("statting PCAS_ROOT {}", canonical_root.display()))?;
        anyhow::ensure!(
            meta.is_dir(),
            "PCAS_ROOT {} is not a directory",
            canonical_root.display()
        );
        // `.pcas` may not have been created yet (e.g. before the first
        // `pcas index`); that is not an error, it just means no target can
        // resolve into it.
        let canonical_pcas = std::fs::canonicalize(canonical_root.join(".pcas")).ok();
        Ok(Self {
            canonical_root,
            canonical_pcas,
        })
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
}

/// A successfully opened regular file and its descriptor-derived metadata.
pub struct OpenFile {
    pub file: tokio::fs::File,
    pub meta: std::fs::Metadata,
}

/// The outcome of resolving a validated visible request path.
pub enum Resolved {
    File(Box<OpenFile>),
    Directory {
        canonical_path: PathBuf,
    },
    /// Missing, outside the root, inside `.pcas` (directly or through a
    /// symlink), or not a regular file/directory. Deliberately not
    /// distinguished further: which of these applies must never be
    /// observable by a client.
    NotFound,
}

/// Resolve already-decoded, already-validated path segments against `root`.
///
/// An empty `segments` resolves to the root directory itself.
pub async fn resolve(root: &Root, segments: &[Vec<u8>]) -> Result<Resolved> {
    let mut candidate = root.canonical_root.clone();
    for segment in segments {
        candidate.push(std::ffi::OsStr::from_bytes(segment));
    }
    finalize(root, candidate).await
}

/// Resolve a named child of an already-resolved, already-contained
/// directory (used for `index.html`), applying the same canonicalization
/// and containment checks.
pub async fn resolve_child(root: &Root, dir: &Path, name: &[u8]) -> Result<Resolved> {
    finalize(root, dir.join(std::ffi::OsStr::from_bytes(name))).await
}

async fn finalize(root: &Root, candidate: PathBuf) -> Result<Resolved> {
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(p) => p,
        // Missing, a broken symlink, a non-directory in the middle of the
        // path, permission denied, a symlink loop, ... all resolve to the
        // same "not found" outcome from the client's point of view.
        Err(_) => return Ok(Resolved::NotFound),
    };
    if !canonical.starts_with(&root.canonical_root) {
        return Ok(Resolved::NotFound);
    }
    if let Some(pcas) = &root.canonical_pcas {
        if canonical.starts_with(pcas) {
            return Ok(Resolved::NotFound);
        }
    }

    let kind = match tokio::fs::metadata(&canonical).await {
        Ok(m) => m,
        Err(_) => return Ok(Resolved::NotFound),
    };
    if kind.is_dir() {
        return Ok(Resolved::Directory {
            canonical_path: canonical,
        });
    }
    if !kind.is_file() {
        // Devices, sockets, FIFOs, ...: never served.
        return Ok(Resolved::NotFound);
    }

    let file = match tokio::fs::File::open(&canonical).await {
        Ok(f) => f,
        Err(_) => return Ok(Resolved::NotFound),
    };
    // From here on the file is open: any further failure is unexpected I/O
    // after successful resolution, not a resolution outcome.
    let meta = file
        .metadata()
        .await
        .context("statting an already-opened file descriptor")?;
    Ok(Resolved::File(Box::new(OpenFile { file, meta })))
}
