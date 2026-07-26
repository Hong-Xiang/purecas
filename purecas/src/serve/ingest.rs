//! Streaming, create-only publication into the visible hierarchy followed by
//! exact-file filesystem-first indexing.

use crate::index::types::{FileSnapshot, RootRelativePath, Sha256Digest};
use crate::index::{self, PendingContent, PendingFile, PendingFileError, PendingPaths};
use crate::serve::path::VisiblePath;
use crate::serve::resolve::Root;
use anyhow::{anyhow, Context};
use axum::body::Body;
use futures_util::StreamExt;
use rustix::fd::OwnedFd;
use rustix::fs::{mkdirat, openat, statat, unlinkat, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::{File, TryLockError};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub struct Ingested {
    pub digest: Sha256Digest,
    pub relative_path: String,
}

pub enum IngestError {
    NotFound,
    Conflict,
    BadBody(anyhow::Error),
    Internal(anyhow::Error),
    IndexBusy(anyhow::Error),
    CrossDevice(anyhow::Error),
    IndexFailed(anyhow::Error),
}

struct Publication {
    parent: OwnedFd,
    name: OsString,
}

struct PreparedDestination {
    relative_path: RootRelativePath,
    encoded_location: String,
    publication: Publication,
    index_root: OwnedFd,
    index_internal_root: OwnedFd,
}

struct StagedUpload {
    file: Option<tokio::fs::File>,
    directory: Option<OwnedFd>,
    name: Option<OsString>,
    lock_name: Option<OsString>,
    lock_file: Option<File>,
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        if let (Some(directory), Some(name)) = (&self.directory, &self.name) {
            let _ = unlinkat(directory, name, AtFlags::empty());
        }
        if let (Some(directory), Some(lock_name)) = (&self.directory, &self.lock_name) {
            let _ = unlinkat(directory, lock_name, AtFlags::empty());
        }
    }
}

impl StagedUpload {
    fn file(&mut self) -> &mut tokio::fs::File {
        self.file.as_mut().expect("staged file is present")
    }

    fn into_parts(mut self) -> (tokio::fs::File, OwnedFd, OsString, OsString, File) {
        (
            self.file.take().expect("staged file is present"),
            self.directory.take().expect("staged directory is present"),
            self.name.take().expect("staged name is present"),
            self.lock_name.take().expect("staged lock name is present"),
            self.lock_file.take().expect("staged lock file is present"),
        )
    }
}

#[derive(Debug)]
enum PrepareError {
    NotFound,
    Conflict,
    Internal(anyhow::Error),
}

impl From<PrepareError> for IngestError {
    fn from(error: PrepareError) -> Self {
        match error {
            PrepareError::NotFound => Self::NotFound,
            PrepareError::Conflict => Self::Conflict,
            PrepareError::Internal(error) => Self::Internal(error),
        }
    }
}

fn errno_error(error: Errno, context: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::from_raw_os_error(error.raw_os_error()))
        .context(context.into())
}

fn open_plain_directory(
    parent: &impl AsFd,
    name: &OsStr,
    create: bool,
) -> Result<OwnedFd, PrepareError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    loop {
        match openat(parent, name, flags, Mode::empty()) {
            Ok(directory) => return Ok(directory),
            Err(Errno::NOENT) if create => match mkdirat(parent, name, Mode::from(0o755)) {
                Ok(()) | Err(Errno::EXIST) => continue,
                Err(error) => {
                    return Err(PrepareError::Internal(errno_error(
                        error,
                        format!("creating directory {:?}", name),
                    )))
                }
            },
            Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => return Err(PrepareError::NotFound),
            Err(error) => {
                return Err(PrepareError::Internal(errno_error(
                    error,
                    format!("opening directory {:?}", name),
                )))
            }
        }
    }
}

enum DestinationState {
    Missing,
    RegularFile,
    Inaccessible,
}

fn destination_state(parent: &impl AsFd, name: &OsStr) -> Result<DestinationState, PrepareError> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_file() => {
            Ok(DestinationState::RegularFile)
        }
        Ok(_) => Ok(DestinationState::Inaccessible),
        Err(Errno::NOENT) => Ok(DestinationState::Missing),
        Err(Errno::LOOP | Errno::NOTDIR) => Ok(DestinationState::Inaccessible),
        Err(error) => Err(PrepareError::Internal(errno_error(
            error,
            format!("checking destination {:?}", name),
        ))),
    }
}

fn prepare_destination(
    root_dir: File,
    parsed: VisiblePath,
) -> Result<(PreparedDestination, OwnedFd), PrepareError> {
    if parsed.segments.is_empty()
        || parsed.trailing_slash
        || parsed.segments.iter().any(|segment| segment == b".")
        || parsed
            .segments
            .first()
            .is_some_and(|segment| segment == b"purecas.db")
    {
        return Err(PrepareError::NotFound);
    }

    match statat(&root_dir, "purecas.db", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {
            return Err(PrepareError::Internal(anyhow!(
                "filesystem-first ingestion refuses a root containing top-level purecas.db"
            )))
        }
        Err(Errno::NOENT) => {}
        Err(error) => {
            return Err(PrepareError::Internal(errno_error(
                error,
                "checking top-level purecas.db",
            )))
        }
    }

    let index_root: OwnedFd = root_dir
        .try_clone()
        .map_err(|error| {
            PrepareError::Internal(
                anyhow::Error::new(error).context("cloning root directory handle"),
            )
        })?
        .into();
    let internal_root = root_dir.try_clone().map_err(|error| {
        PrepareError::Internal(anyhow::Error::new(error).context("cloning root directory handle"))
    })?;
    let mut parent: OwnedFd = root_dir.into();
    for segment in &parsed.segments[..parsed.segments.len() - 1] {
        parent = open_plain_directory(&parent, OsStr::from_bytes(segment), true)?;
    }

    let destination_name = OsString::from(OsStr::from_bytes(
        parsed
            .segments
            .last()
            .expect("non-empty path was checked above"),
    ));
    match destination_state(&parent, &destination_name)? {
        DestinationState::Missing => {}
        DestinationState::RegularFile => return Err(PrepareError::Conflict),
        DestinationState::Inaccessible => return Err(PrepareError::NotFound),
    }

    let mut relative = PathBuf::new();
    for segment in &parsed.segments {
        relative.push(OsStr::from_bytes(segment));
    }
    let relative_path =
        RootRelativePath::from_relative(&relative).map_err(PrepareError::Internal)?;
    let internal_root: OwnedFd = internal_root.into();
    let dot_pcas = open_plain_directory(&internal_root, OsStr::new(".pcas"), true).map_err(
        |error| match error {
            PrepareError::NotFound => {
                PrepareError::Internal(anyhow!("top-level .pcas is not a plain directory"))
            }
            other => other,
        },
    )?;
    let upload_tmp = open_plain_directory(&dot_pcas, OsStr::new("ingest-tmp"), true).map_err(
        |error| match error {
            PrepareError::NotFound => {
                PrepareError::Internal(anyhow!(".pcas/ingest-tmp is not a plain directory"))
            }
            other => other,
        },
    )?;

    Ok((
        PreparedDestination {
            relative_path,
            encoded_location: parsed.encoded_path(),
            publication: Publication {
                parent,
                name: destination_name,
            },
            index_root,
            index_internal_root: dot_pcas,
        },
        upload_tmp,
    ))
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temporary_stem() -> String {
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{counter:x}-{nanos:x}", std::process::id())
}

fn cleanup_stale_uploads(directory: &OwnedFd) -> Result<(), PrepareError> {
    let path = PathBuf::from("/proc/self/fd").join(directory.as_fd().as_raw_fd().to_string());
    let entries = std::fs::read_dir(&path)
        .with_context(|| format!("reading {}", path.display()))
        .map_err(PrepareError::Internal)?;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("reading {}", path.display()))
            .map_err(PrepareError::Internal)?;
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let Some(stem) = name_text.strip_suffix(".lock") else {
            continue;
        };
        let lock = match openat(
            directory,
            &name,
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(lock) => File::from(lock),
            Err(Errno::NOENT) => continue,
            Err(error) => {
                return Err(PrepareError::Internal(errno_error(
                    error,
                    "opening stale-upload lock",
                )))
            }
        };
        match lock.try_lock() {
            Ok(()) => {
                let body_name = format!("{stem}.upload");
                let _ = unlinkat(directory, &body_name, AtFlags::empty());
                let _ = unlinkat(directory, &name, AtFlags::empty());
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(PrepareError::Internal(
                    anyhow::Error::new(error).context("locking stale-upload lock"),
                ))
            }
        }
    }

    let entries = std::fs::read_dir(&path)
        .with_context(|| format!("re-reading {}", path.display()))
        .map_err(PrepareError::Internal)?;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("reading {}", path.display()))
            .map_err(PrepareError::Internal)?;
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let Some(stem) = name_text.strip_suffix(".upload") else {
            continue;
        };
        let lock_name = format!("{stem}.lock");
        if statat(directory, &lock_name, AtFlags::SYMLINK_NOFOLLOW).is_err() {
            let _ = unlinkat(directory, &name, AtFlags::empty());
        }
    }
    Ok(())
}

pub(crate) fn cleanup_stale(root: &Root) -> anyhow::Result<()> {
    let root_dir = root
        .try_clone_dir()
        .context("cloning root directory handle for ingestion cleanup")?;
    let root_dir: OwnedFd = root_dir.into();
    let dot_pcas =
        open_plain_directory(&root_dir, OsStr::new(".pcas"), true).map_err(
            |error| match error {
                PrepareError::Internal(error) => error,
                PrepareError::NotFound => anyhow!("top-level .pcas is not a plain directory"),
                PrepareError::Conflict => anyhow!("unexpected ingestion cleanup conflict"),
            },
        )?;
    let upload_tmp = open_plain_directory(&dot_pcas, OsStr::new("ingest-tmp"), true).map_err(
        |error| match error {
            PrepareError::Internal(error) => error,
            PrepareError::NotFound => anyhow!(".pcas/ingest-tmp is not a plain directory"),
            PrepareError::Conflict => anyhow!("unexpected ingestion cleanup conflict"),
        },
    )?;
    cleanup_stale_uploads(&upload_tmp).map_err(|error| match error {
        PrepareError::Internal(error) => error,
        PrepareError::NotFound => anyhow!("ingestion cleanup path disappeared"),
        PrepareError::Conflict => anyhow!("unexpected ingestion cleanup conflict"),
    })
}

fn create_staged_upload(directory: OwnedFd) -> Result<StagedUpload, PrepareError> {
    cleanup_stale_uploads(&directory)?;
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    for _ in 0..16 {
        let stem = temporary_stem();
        let name = OsString::from(format!("{stem}.upload"));
        let lock_name = OsString::from(format!("{stem}.lock"));
        let lock = match openat(&directory, &lock_name, flags, Mode::from(0o600)) {
            Ok(lock) => File::from(lock),
            Err(Errno::EXIST) => continue,
            Err(error) => {
                return Err(PrepareError::Internal(errno_error(
                    error,
                    "creating ingestion lock file",
                )))
            }
        };
        if let Err(error) = lock.try_lock() {
            let _ = unlinkat(&directory, &lock_name, AtFlags::empty());
            return Err(PrepareError::Internal(anyhow!(
                "locking new ingestion lock file: {error}"
            )));
        }
        match openat(&directory, &name, flags, Mode::from(0o600)) {
            Ok(file) => {
                return Ok(StagedUpload {
                    file: Some(tokio::fs::File::from_std(File::from(file))),
                    directory: Some(directory),
                    name: Some(name),
                    lock_name: Some(lock_name),
                    lock_file: Some(lock),
                })
            }
            Err(Errno::EXIST) => {
                let _ = unlinkat(&directory, &lock_name, AtFlags::empty());
                continue;
            }
            Err(error) => {
                let _ = unlinkat(&directory, &lock_name, AtFlags::empty());
                return Err(PrepareError::Internal(errno_error(
                    error,
                    "creating ingestion temporary file",
                )));
            }
        }
    }
    Err(PrepareError::Internal(anyhow!(
        "could not allocate a unique ingestion temporary file"
    )))
}

pub async fn ingest(root: &Root, parsed: VisiblePath, body: Body) -> Result<Ingested, IngestError> {
    let root_path = root.canonical_root().to_path_buf();
    let root_dir = root
        .try_clone_dir()
        .context("cloning root directory handle")
        .map_err(IngestError::Internal)?;
    let (prepared, mut staged) = tokio::task::spawn_blocking(move || {
        let (prepared, upload_tmp) = prepare_destination(root_dir, parsed)?;
        let staged = create_staged_upload(upload_tmp)?;
        Ok::<_, PrepareError>((prepared, staged))
    })
    .await
    .map_err(|error| IngestError::Internal(anyhow::Error::new(error)))??;

    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| IngestError::BadBody(anyhow::Error::new(error)))?;
        hasher.update(&chunk);
        staged
            .file()
            .write_all(&chunk)
            .await
            .context("writing ingestion request body")
            .map_err(IngestError::Internal)?;
    }
    staged
        .file()
        .flush()
        .await
        .context("flushing ingestion temporary file")
        .map_err(IngestError::Internal)?;
    staged
        .file()
        .sync_all()
        .await
        .context("syncing ingestion temporary file")
        .map_err(IngestError::Internal)?;
    let expected = FileSnapshot::from_metadata(
        &staged
            .file()
            .metadata()
            .await
            .context("statting synced ingestion temporary file")
            .map_err(IngestError::Internal)?,
    );
    let expected_digest = Sha256Digest::parse(&format!("{:x}", hasher.finalize()))
        .expect("SHA-256 output is always a valid digest");
    let (staged_file, source_parent, source_name, source_lock_name, source_lock) =
        staged.into_parts();
    let staged_file = staged_file.into_std().await;

    let PreparedDestination {
        relative_path,
        encoded_location,
        publication,
        index_root,
        index_internal_root,
    } = prepared;
    let pending = PendingFile::new(
        index_root,
        index_internal_root,
        PendingPaths::new(
            source_parent,
            source_name,
            source_lock_name,
            source_lock,
            publication.parent,
            publication.name,
        ),
        relative_path,
        root_path,
        staged_file,
        PendingContent::new(expected, expected_digest),
    );
    let indexed = tokio::task::spawn_blocking(move || {
        index::index_and_publish_pending(pending, Duration::from_secs(1))
    })
    .await
    .map_err(|error| IngestError::Internal(anyhow::Error::new(error)))?;
    let encoded_relative = encoded_location
        .strip_prefix('/')
        .expect("encoded hierarchy locations start with slash")
        .to_string();

    match indexed {
        Ok(indexed) => Ok(Ingested {
            digest: indexed.digest,
            relative_path: encoded_relative,
        }),
        Err(PendingFileError::LockBusy(error)) => Err(IngestError::IndexBusy(error)),
        Err(PendingFileError::Conflict) => Err(IngestError::Conflict),
        Err(PendingFileError::Inaccessible) => Err(IngestError::NotFound),
        Err(PendingFileError::CrossDevice(error)) => Err(IngestError::CrossDevice(error)),
        Err(PendingFileError::Failed(error)) => Err(IngestError::IndexFailed(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use tempfile::TempDir;

    async fn pending_with_mutator(
        prepared: PreparedDestination,
        mut staged: StagedUpload,
        display_root: PathBuf,
        content: &[u8],
    ) -> (PendingFile, File) {
        staged.file().write_all(content).await.unwrap();
        staged.file().sync_all().await.unwrap();
        let snapshot = FileSnapshot::from_metadata(&staged.file().metadata().await.unwrap());
        let digest = Sha256Digest::parse(&format!("{:x}", Sha256::digest(content))).unwrap();
        let (file, source_parent, source_name, source_lock_name, source_lock) = staged.into_parts();
        let file = file.into_std().await;
        let mutator = file.try_clone().unwrap();
        (
            PendingFile::new(
                prepared.index_root,
                prepared.index_internal_root,
                PendingPaths::new(
                    source_parent,
                    source_name,
                    source_lock_name,
                    source_lock,
                    prepared.publication.parent,
                    prepared.publication.name,
                ),
                prepared.relative_path,
                display_root,
                file,
                PendingContent::new(snapshot, digest),
            ),
            mutator,
        )
    }

    async fn pending(
        prepared: PreparedDestination,
        staged: StagedUpload,
        display_root: PathBuf,
        content: &[u8],
    ) -> PendingFile {
        pending_with_mutator(prepared, staged, display_root, content)
            .await
            .0
    }

    #[tokio::test]
    async fn held_descriptors_prevent_symlink_swap_escape_and_false_success() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        let parsed = crate::serve::path::parse("/parent/file.bin").unwrap();
        let root_dir = File::open(root.path()).unwrap();
        let (prepared, upload_tmp) = prepare_destination(root_dir, parsed).unwrap();
        let staged = create_staged_upload(upload_tmp).unwrap();

        std::fs::rename(root.path().join("parent"), root.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("parent")).unwrap();

        let pending = pending(prepared, staged, root.path().to_path_buf(), b"content").await;
        let error = index::index_and_publish_pending(pending, Duration::ZERO).unwrap_err();

        assert!(matches!(error, PendingFileError::Failed(_)));
        assert!(!root.path().join("moved/file.bin").exists());
        assert!(!outside.path().join("file.bin").exists());
        let digest = format!("{:x}", Sha256::digest(b"content"));
        assert!(matches!(
            crate::index::resolve_digest(root.path(), &digest),
            Err(crate::index::DigestResolutionError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn replaced_configured_root_path_rejects_false_success() {
        let container = TempDir::new().unwrap();
        let root = container.path().join("root");
        let moved = container.path().join("moved");
        std::fs::create_dir(&root).unwrap();
        let parsed = crate::serve::path::parse("/file.bin").unwrap();
        let root_dir = File::open(&root).unwrap();
        let (prepared, upload_tmp) = prepare_destination(root_dir, parsed).unwrap();
        let staged = create_staged_upload(upload_tmp).unwrap();

        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();

        let pending = pending(prepared, staged, root, b"content").await;
        let error = index::index_and_publish_pending(pending, Duration::ZERO).unwrap_err();

        assert!(matches!(error, PendingFileError::Failed(_)));
        assert!(!moved.join("file.bin").exists());
        assert!(!moved.join(".pcas/sha256").exists());
    }

    #[tokio::test]
    async fn source_snapshot_is_revalidated_after_lock_wait() {
        let root = TempDir::new().unwrap();
        let parsed = crate::serve::path::parse("/waiting.bin").unwrap();
        let root_dir = File::open(root.path()).unwrap();
        let (prepared, upload_tmp) = prepare_destination(root_dir, parsed).unwrap();
        let staged = create_staged_upload(upload_tmp).unwrap();
        let (pending, mut mutator) =
            pending_with_mutator(prepared, staged, root.path().to_path_buf(), b"initial").await;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.path().join(".pcas/index.lock"))
            .unwrap();
        lock.try_lock().unwrap();

        let indexing = tokio::task::spawn_blocking(move || {
            index::index_and_publish_pending(pending, Duration::from_secs(1))
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        mutator.seek(SeekFrom::Start(0)).unwrap();
        mutator.write_all(b"changed").unwrap();
        mutator.sync_all().unwrap();
        drop(lock);

        let error = indexing.await.unwrap().unwrap_err();

        assert!(matches!(error, PendingFileError::Failed(_)));
        assert!(!root.path().join("waiting.bin").exists());
        assert_eq!(
            std::fs::read_dir(root.path().join(".pcas/ingest-tmp"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn stale_ingestion_cleanup_skips_active_and_reclaims_crashed_uploads() {
        let root = TempDir::new().unwrap();
        let internal = root.path().join(".pcas/ingest-tmp");
        std::fs::create_dir_all(&internal).unwrap();
        let lock_path = internal.join("stale.lock");
        let body_path = internal.join("stale.upload");
        std::fs::write(&lock_path, b"").unwrap();
        std::fs::write(&body_path, b"partial").unwrap();
        let lock = OpenOptions::new().write(true).open(&lock_path).unwrap();
        lock.try_lock().unwrap();
        let opened = Root::open(root.path()).unwrap();

        cleanup_stale(&opened).unwrap();
        assert!(lock_path.exists());
        assert!(body_path.exists());

        drop(lock);
        cleanup_stale(&opened).unwrap();
        assert!(!lock_path.exists());
        assert!(!body_path.exists());
    }
}
