//! Representation metadata derived from a single opened file descriptor:
//! the weak `ETag` and `Last-Modified` value shared by response headers and
//! conditional-request evaluation.

use headers::ETag;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Representation metadata for one opened regular file.
#[derive(Debug, Clone)]
pub struct Representation {
    pub len: u64,
    pub last_modified: SystemTime,
    pub etag: ETag,
}

impl Representation {
    /// Derive representation metadata from a descriptor's own `fstat`
    /// result. The weak `ETag` is exactly `W/"<device>-<inode>-<size>-<mtime_ns>"`,
    /// where `mtime_ns` is the modification time as whole nanoseconds since
    /// the Unix epoch (`mtime * 1_000_000_000 + mtime_nsec`).
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        let dev = meta.dev();
        let ino = meta.ino();
        let len = meta.size();
        let mtime_secs = meta.mtime();
        let mtime_nsec = meta.mtime_nsec();
        let mtime_ns = i128::from(mtime_secs) * 1_000_000_000 + i128::from(mtime_nsec);

        let etag_value = format!("W/\"{dev}-{ino}-{len}-{mtime_ns}\"");
        let etag = etag_value
            .parse::<ETag>()
            .expect("device/inode/size/mtime fields never produce invalid ETag syntax");

        let last_modified = if mtime_secs >= 0 {
            UNIX_EPOCH + Duration::new(mtime_secs as u64, mtime_nsec as u32)
        } else {
            // A modification time before the Unix epoch is not realistic
            // for served content; clamp rather than panic on the
            // `Duration` conversion.
            UNIX_EPOCH
        };

        Self {
            len,
            last_modified,
            etag,
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
    fn etag_format_matches_spec() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        let meta = f.as_file().metadata().unwrap();
        let repr = Representation::from_metadata(&meta);
        let etag_str = etag_wire_form(&repr.etag);
        assert!(etag_str.starts_with("W/\""), "{etag_str}");
        let inner = etag_str.trim_start_matches("W/\"").trim_end_matches('"');
        let parts: Vec<&str> = inner.split('-').collect();
        assert_eq!(parts.len(), 4, "{etag_str}");
        assert_eq!(parts[0], meta.dev().to_string());
        assert_eq!(parts[1], meta.ino().to_string());
        assert_eq!(parts[2], meta.size().to_string());
    }
}
