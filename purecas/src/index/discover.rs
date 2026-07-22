//! Recursive discovery of visible regular files under `PCAS_ROOT`, with
//! basename/relative-path pattern matching.

use super::types::RootRelativePath;
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobBuilder, GlobMatcher};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// A validated selection pattern for `pcas index [PATTERN]`.
pub enum Pattern {
    /// No pattern: every visible regular file is selected.
    All,
    /// A pattern without `/`: matches basenames recursively.
    Basename(GlobMatcher),
    /// A pattern containing `/`: matches root-relative paths.
    RelativePath(GlobMatcher),
}

impl Pattern {
    /// Parse and validate a raw `PATTERN` argument. Rejects absolute and
    /// parent-escaping patterns.
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw else {
            return Ok(Self::All);
        };
        if raw.is_empty() {
            bail!("pattern must not be empty");
        }
        if raw.starts_with('/') {
            bail!("pattern must not be absolute: {raw:?}");
        }
        let escapes = Path::new(raw)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)));
        if escapes {
            bail!("pattern must not contain parent-directory segments: {raw:?}");
        }

        if raw.contains('/') {
            Ok(Self::RelativePath(compile(raw)?))
        } else {
            Ok(Self::Basename(compile(raw)?))
        }
    }

    /// Test a discovered root-relative path against this pattern.
    pub fn matches(&self, rel: &RootRelativePath) -> bool {
        match self {
            Self::All => true,
            Self::Basename(glob) => rel
                .as_path()
                .file_name()
                .is_some_and(|name| glob.is_match(name)),
            Self::RelativePath(glob) => glob.is_match(rel.as_path()),
        }
    }
}

fn compile(raw: &str) -> Result<GlobMatcher> {
    let glob: Glob = GlobBuilder::new(raw)
        .literal_separator(true)
        .build()
        .with_context(|| format!("invalid glob pattern: {raw:?}"))?;
    Ok(glob.compile_matcher())
}

/// Recursively discover every visible regular file under `root`.
///
/// `.pcas` is pruned before descent; symbolic links are never followed and
/// are never themselves treated as regular files; directories, sockets,
/// devices, and FIFOs are skipped.
pub fn discover_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".pcas");

    for entry in walker {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if entry.path_is_symlink() {
            continue;
        }
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}
