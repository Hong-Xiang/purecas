//! Representation metadata shared by the visible-hierarchy and digest
//! routes: `ETag` strength, `Last-Modified`, length, MIME type, and cache
//! policy, all derived once from a single opened file descriptor's own
//! `fstat` result (plus, for the digest route, the already-resolved
//! digest and packed suffix).

use crate::index::types::{FileTypeSuffix, Sha256Digest};
use headers::ETag;
use http::HeaderValue;
use mime_guess::Mime;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The `Cache-Control` policy for one representation. Rendered directly
/// as a raw header value rather than through `headers::CacheControl`,
/// whose fixed directive ordering cannot produce the exact
/// `public, max-age=31536000, immutable` text the digest route requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// The visible hierarchy: content at a given path can change, so
    /// every response must be revalidated.
    NoCache,
    /// The digest route: content is immutable by contract (in-place
    /// mutation is store damage, repaired by `pcas index`), so it can be
    /// cached by any cache indefinitely.
    Immutable,
}

impl CachePolicy {
    pub fn header_value(self) -> HeaderValue {
        match self {
            Self::NoCache => HeaderValue::from_static("no-cache"),
            Self::Immutable => HeaderValue::from_static("public, max-age=31536000, immutable"),
        }
    }
}

/// Representation metadata for one opened regular file.
#[derive(Debug, Clone)]
pub struct Representation {
    pub len: u64,
    pub last_modified: SystemTime,
    pub etag: ETag,
    pub mime: Mime,
    pub cache_policy: CachePolicy,
    /// A stable, content-identity string, used only to derive the
    /// deterministic multipart boundary. Never emitted directly in any
    /// response header.
    pub identity: String,
}

fn last_modified_from(mtime_secs: i64, mtime_nsec: i64) -> SystemTime {
    if mtime_secs >= 0 {
        UNIX_EPOCH + Duration::new(mtime_secs as u64, mtime_nsec as u32)
    } else {
        // A modification time before the Unix epoch is not realistic for
        // served content; clamp rather than panic on the `Duration`
        // conversion.
        UNIX_EPOCH
    }
}

impl Representation {
    /// Build representation metadata for one visible-hierarchy file: a
    /// weak `ETag` of `W/"<device>-<inode>-<size>-<mtime_ns>"` (so a
    /// content-identical replacement of the same path is still a new
    /// representation), `Cache-Control: no-cache`, and a MIME type
    /// guessed from the requested (visible) file name.
    pub fn for_hierarchy(meta: &std::fs::Metadata, requested_name: &[u8]) -> Self {
        let dev = meta.dev();
        let ino = meta.ino();
        let len = meta.size();
        let mtime_secs = meta.mtime();
        let mtime_nsec = meta.mtime_nsec();
        let mtime_ns = i128::from(mtime_secs) * 1_000_000_000 + i128::from(mtime_nsec);

        let identity = format!("{dev}-{ino}-{len}-{mtime_ns}");
        let etag = format!("W/\"{identity}\"")
            .parse::<ETag>()
            .expect("device/inode/size/mtime fields never produce invalid ETag syntax");
        let mime = mime_guess::from_path(Path::new(OsStr::from_bytes(requested_name)))
            .first_or_octet_stream();

        Self {
            len,
            last_modified: last_modified_from(mtime_secs, mtime_nsec),
            etag,
            mime,
            cache_policy: CachePolicy::NoCache,
            identity,
        }
    }

    /// Build representation metadata for one resolved digest object: a
    /// strong `ETag` of exactly `"<lowercase-digest>"`,
    /// `Cache-Control: public, max-age=31536000, immutable`, and a MIME
    /// type guessed from the packed object's suffix (falling back to
    /// `application/octet-stream` when there is none).
    pub fn for_digest(
        meta: &std::fs::Metadata,
        digest: &Sha256Digest,
        suffix: Option<&FileTypeSuffix>,
    ) -> Self {
        let identity = digest.as_str().to_string();
        let etag = format!("\"{identity}\"")
            .parse::<ETag>()
            .expect("a 64-hex-character digest never produces invalid ETag syntax");
        let mime = match suffix {
            Some(suffix) => mime_guess::from_path(format!("x.{}", suffix.as_str())),
            None => mime_guess::from_path("x"),
        }
        .first_or_octet_stream();

        Self {
            len: meta.size(),
            last_modified: last_modified_from(meta.mtime(), meta.mtime_nsec()),
            etag,
            mime,
            cache_policy: CachePolicy::Immutable,
            identity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headers::HeaderMapExt;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// `headers::ETag` has no `Display` impl; round-trip it through a
    /// `HeaderMap` to inspect the wire form it encodes to.
    fn etag_wire_form(etag: &ETag) -> String {
        let mut headers = http::HeaderMap::new();
        headers.typed_insert(etag.clone());
        headers
            .get(http::header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn hierarchy_etag_format_matches_spec() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        let meta = f.as_file().metadata().unwrap();
        let repr = Representation::for_hierarchy(&meta, b"hello.txt");
        let etag_str = etag_wire_form(&repr.etag);
        assert!(etag_str.starts_with("W/\""), "{etag_str}");
        let inner = etag_str.trim_start_matches("W/\"").trim_end_matches('"');
        let parts: Vec<&str> = inner.split('-').collect();
        assert_eq!(parts.len(), 4, "{etag_str}");
        assert_eq!(parts[0], meta.dev().to_string());
        assert_eq!(parts[1], meta.ino().to_string());
        assert_eq!(parts[2], meta.size().to_string());
        assert_eq!(repr.cache_policy.header_value(), "no-cache");
    }

    #[test]
    fn digest_etag_is_exactly_the_lowercase_digest() {
        let f = NamedTempFile::new().unwrap();
        let meta = f.as_file().metadata().unwrap();
        let digest = Sha256Digest::parse(&"a".repeat(64)).unwrap();
        let repr = Representation::for_digest(&meta, &digest, None);
        let etag_str = etag_wire_form(&repr.etag);
        assert_eq!(etag_str, format!("\"{}\"", "a".repeat(64)));
        assert_eq!(
            repr.cache_policy.header_value(),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(repr.mime, mime_guess::mime::APPLICATION_OCTET_STREAM);
    }

    #[test]
    fn digest_mime_comes_from_packed_suffix() {
        let f = NamedTempFile::new().unwrap();
        let meta = f.as_file().metadata().unwrap();
        let digest = Sha256Digest::parse(&"b".repeat(64)).unwrap();
        let suffix = FileTypeSuffix::parse("mp4").unwrap();
        let repr = Representation::for_digest(&meta, &digest, Some(&suffix));
        assert_eq!(repr.mime.essence_str(), "video/mp4");
    }
}
