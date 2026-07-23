//! Strict domain types for the filesystem-first object index.
//!
//! Every value that crosses a boundary of the packed object layout
//! (`.pcas/sha256/<first2>/<hash>--<timestamp>[.<suffix>]`) is parsed once
//! into one of these types; nothing downstream reconstructs meaning from
//! raw strings or path slicing.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// A validated, lowercase, 64-hex-character SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parse and normalize a digest. Accepts any hex case; rejects wrong
    /// length or non-hex characters.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() != 64 {
            bail!(
                "digest must be exactly 64 hex characters, got {} in {raw:?}",
                raw.len()
            );
        }
        if !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("digest must be hexadecimal: {raw:?}");
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    /// The full lowercase hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The two-character shard directory name for this digest.
    pub fn shard(&self) -> &str {
        &self.0[..2]
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A UTC "basic" ISO 8601 index timestamp: `YYYYMMDDTHHMMSS[.fffffffff]Z`.
///
/// The original fractional-digit width (if any) is preserved so that
/// parsing and formatting round-trip byte-for-byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexTimestamp {
    at: DateTime<Utc>,
    fraction_digits: Option<String>,
}

impl IndexTimestamp {
    /// The current UTC instant, encoded with trimmed nanosecond precision
    /// (no trailing zero digits; omitted entirely when exactly zero).
    pub fn now() -> Self {
        let at = Utc::now();
        let nanos = at.timestamp_subsec_nanos();
        let fraction_digits = if nanos == 0 {
            None
        } else {
            let padded = format!("{nanos:09}");
            let trimmed = padded.trim_end_matches('0');
            Some(trimmed.to_string())
        };
        Self {
            at,
            fraction_digits,
        }
    }

    /// Parse a timestamp through its terminating `Z`. `raw` must contain
    /// nothing after the `Z`.
    pub fn parse(raw: &str) -> Result<Self> {
        let body = raw
            .strip_suffix('Z')
            .with_context(|| format!("timestamp must end with 'Z': {raw:?}"))?;
        let (date_time_part, fraction_part) = match body.split_once('.') {
            Some((dt, frac)) => (dt, Some(frac)),
            None => (body, None),
        };
        if date_time_part.len() != 15 {
            bail!("timestamp must have the form YYYYMMDDTHHMMSS[.fffffffff]Z: {raw:?}");
        }
        let naive = NaiveDateTime::parse_from_str(date_time_part, "%Y%m%dT%H%M%S")
            .with_context(|| format!("parsing timestamp {raw:?}"))?;

        let fraction_digits = match fraction_part {
            None => None,
            Some(f) => {
                if f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
                    bail!("fractional seconds must be 1-9 digits: {raw:?}");
                }
                Some(f.to_string())
            }
        };
        let nanos: u32 = match &fraction_digits {
            None => 0,
            Some(f) => {
                let mut padded = f.clone();
                padded.push_str(&"0".repeat(9 - f.len()));
                padded
                    .parse()
                    .with_context(|| format!("parsing fractional seconds: {raw:?}"))?
            }
        };
        let naive = naive
            .with_nanosecond(nanos)
            .with_context(|| format!("invalid fractional seconds: {raw:?}"))?;
        let at = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        Ok(Self {
            at,
            fraction_digits,
        })
    }

    /// Format back to the canonical `YYYYMMDDTHHMMSS[.fffffffff]Z` text.
    pub fn format(&self) -> String {
        let base = self.at.format("%Y%m%dT%H%M%S");
        match &self.fraction_digits {
            None => format!("{base}Z"),
            Some(f) => format!("{base}.{f}Z"),
        }
    }
}

impl fmt::Display for IndexTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

/// A sanitized, lowercase-ASCII file-type suffix hint, e.g. `txt` or
/// `tar.gz`. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTypeSuffix(String);

fn is_safe_segment_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
}

impl FileTypeSuffix {
    /// Parse an already-encoded suffix (as found in a packed object file
    /// name). Accepts simple (`txt`) and compound (`tar.gz`) forms.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.is_empty() {
            bail!("suffix must not be empty");
        }
        let valid = raw
            .split('.')
            .all(|segment| !segment.is_empty() && segment.chars().all(is_safe_segment_char));
        if !valid {
            bail!(
                "suffix must be one or more '.'-separated lowercase ascii [a-z0-9_-] segments: {raw:?}"
            );
        }
        Ok(Self(raw.to_string()))
    }

    /// Infer a suffix from a source file's basename, using only its final
    /// extension. Dotfiles (a leading dot with no further dot) and empty
    /// or unsanitizable extensions infer no suffix.
    pub fn infer_from_file_name(file_name: &str) -> Option<Self> {
        let is_dotfile_without_extension =
            file_name.starts_with('.') && !file_name[1..].contains('.');
        if is_dotfile_without_extension {
            return None;
        }
        let (_, ext) = file_name.rsplit_once('.')?;
        if ext.is_empty() {
            return None;
        }
        let sanitized: String = ext
            .chars()
            .map(|c| c.to_ascii_lowercase())
            .filter(|c| is_safe_segment_char(*c))
            .collect();
        if sanitized.is_empty() {
            None
        } else {
            Some(Self(sanitized))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileTypeSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A packed object file name: `<digest>--<timestamp>[.<suffix>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectFileName {
    digest: Sha256Digest,
    timestamp: IndexTimestamp,
    suffix: Option<FileTypeSuffix>,
}

impl ObjectFileName {
    pub fn new(
        digest: Sha256Digest,
        timestamp: IndexTimestamp,
        suffix: Option<FileTypeSuffix>,
    ) -> Self {
        Self {
            digest,
            timestamp,
            suffix,
        }
    }

    /// Parse left-to-right: fixed 64-hex digest, `--`, timestamp through its
    /// terminating `Z`, then an optional `.<suffix>`.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() < 64 {
            bail!("object file name is too short to contain a digest: {raw:?}");
        }
        let (digest_part, rest) = raw.split_at(64);
        let digest = Sha256Digest::parse(digest_part)
            .with_context(|| format!("parsing digest in object file name {raw:?}"))?;

        let rest = rest
            .strip_prefix("--")
            .with_context(|| format!("expected '--' after digest in {raw:?}"))?;

        let z_pos = rest
            .find('Z')
            .with_context(|| format!("missing 'Z' timestamp terminator in {raw:?}"))?;
        let (timestamp_str, remainder) = rest.split_at(z_pos + 1);
        let timestamp = IndexTimestamp::parse(timestamp_str)
            .with_context(|| format!("parsing timestamp in object file name {raw:?}"))?;

        let suffix = if remainder.is_empty() {
            None
        } else {
            let suffix_str = remainder
                .strip_prefix('.')
                .with_context(|| format!("expected '.' before suffix in {raw:?}"))?;
            Some(
                FileTypeSuffix::parse(suffix_str)
                    .with_context(|| format!("parsing suffix in object file name {raw:?}"))?,
            )
        };

        Ok(Self {
            digest,
            timestamp,
            suffix,
        })
    }

    pub fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    pub fn timestamp(&self) -> &IndexTimestamp {
        &self.timestamp
    }

    pub fn suffix(&self) -> Option<&FileTypeSuffix> {
        self.suffix.as_ref()
    }

    /// Render the packed file name text.
    pub fn to_file_name(&self) -> String {
        match &self.suffix {
            Some(suffix) => format!("{}--{}.{}", self.digest, self.timestamp, suffix),
            None => format!("{}--{}", self.digest, self.timestamp),
        }
    }
}

impl fmt::Display for ObjectFileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_file_name())
    }
}

/// A path known to be a normalized, relative descendant of `PCAS_ROOT`:
/// non-empty, with no `..` or absolute components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRelativePath(PathBuf);

impl RootRelativePath {
    /// Derive a root-relative path from an absolute path known to be under
    /// `root`.
    pub fn from_root(root: &Path, absolute: &Path) -> Result<Self> {
        let rel = absolute
            .strip_prefix(root)
            .with_context(|| format!("{} is not under {}", absolute.display(), root.display()))?;
        Self::from_relative(rel)
    }

    /// Validate an already-relative path string or path, rejecting absolute
    /// and parent-escaping forms.
    pub fn from_relative(rel: &Path) -> Result<Self> {
        if rel.as_os_str().is_empty() {
            bail!("path must not be empty");
        }
        for component in rel.components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!(
                        "path must be relative and must not escape the root: {}",
                        rel.display()
                    );
                }
            }
        }
        Ok(Self(rel.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for RootRelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_digest() -> &'static str {
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    }

    #[test]
    fn digest_parse_normalizes_case() {
        let upper = sample_digest().to_ascii_uppercase();
        let d = Sha256Digest::parse(&upper).unwrap();
        assert_eq!(d.as_str(), sample_digest());
        assert_eq!(d.shard(), &sample_digest()[..2]);
    }

    #[test]
    fn digest_parse_rejects_wrong_length() {
        assert!(Sha256Digest::parse("abcd").is_err());
        assert!(Sha256Digest::parse(&"a".repeat(65)).is_err());
    }

    #[test]
    fn digest_parse_rejects_non_hex() {
        let mut bad = sample_digest().to_string();
        bad.replace_range(0..1, "g");
        assert!(Sha256Digest::parse(&bad).is_err());
    }

    #[test]
    fn timestamp_round_trips_fractional() {
        let raw = "20260722T130016.139Z";
        let ts = IndexTimestamp::parse(raw).unwrap();
        assert_eq!(ts.format(), raw);
    }

    #[test]
    fn timestamp_round_trips_nonfractional() {
        let raw = "20260722T130016Z";
        let ts = IndexTimestamp::parse(raw).unwrap();
        assert_eq!(ts.format(), raw);
    }

    #[test]
    fn timestamp_round_trips_full_nanosecond_precision() {
        let raw = "20260722T130016.123456789Z";
        let ts = IndexTimestamp::parse(raw).unwrap();
        assert_eq!(ts.format(), raw);
    }

    #[test]
    fn timestamp_rejects_missing_terminal_z() {
        assert!(IndexTimestamp::parse("20260722T130016").is_err());
    }

    #[test]
    fn timestamp_rejects_invalid_calendar_date() {
        assert!(IndexTimestamp::parse("20261332T130016Z").is_err());
    }

    #[test]
    fn timestamp_rejects_empty_fraction() {
        assert!(IndexTimestamp::parse("20260722T130016.Z").is_err());
    }

    #[test]
    fn timestamp_rejects_overlong_fraction() {
        assert!(IndexTimestamp::parse("20260722T130016.1234567890Z").is_err());
    }

    #[test]
    fn timestamp_now_round_trips_through_format_and_parse() {
        let now = IndexTimestamp::now();
        let text = now.format();
        let reparsed = IndexTimestamp::parse(&text).unwrap();
        assert_eq!(reparsed.format(), text);
    }

    #[test]
    fn suffix_infer_dotfile_has_no_suffix() {
        assert_eq!(FileTypeSuffix::infer_from_file_name(".bashrc"), None);
    }

    #[test]
    fn suffix_infer_uses_final_extension_only() {
        let s = FileTypeSuffix::infer_from_file_name("archive.tar.gz").unwrap();
        assert_eq!(s.as_str(), "gz");
    }

    #[test]
    fn suffix_infer_lowercases_and_sanitizes() {
        let s = FileTypeSuffix::infer_from_file_name("Data.TXT").unwrap();
        assert_eq!(s.as_str(), "txt");
    }

    #[test]
    fn suffix_infer_no_extension_is_none() {
        assert_eq!(FileTypeSuffix::infer_from_file_name("README"), None);
    }

    #[test]
    fn suffix_infer_trailing_dot_is_none() {
        assert_eq!(FileTypeSuffix::infer_from_file_name("data."), None);
    }

    #[test]
    fn suffix_infer_dotfile_with_extension_still_infers() {
        let s = FileTypeSuffix::infer_from_file_name(".hidden.json").unwrap();
        assert_eq!(s.as_str(), "json");
    }

    #[test]
    fn suffix_parse_accepts_compound() {
        let s = FileTypeSuffix::parse("tar.gz").unwrap();
        assert_eq!(s.as_str(), "tar.gz");
    }

    #[test]
    fn suffix_parse_rejects_empty() {
        assert!(FileTypeSuffix::parse("").is_err());
    }

    #[test]
    fn suffix_parse_rejects_uppercase() {
        assert!(FileTypeSuffix::parse("TXT").is_err());
    }

    #[test]
    fn suffix_parse_rejects_empty_segment() {
        assert!(FileTypeSuffix::parse("tar.").is_err());
        assert!(FileTypeSuffix::parse(".gz").is_err());
    }

    fn make_object_file_name(digest: &str, timestamp: &str, suffix: Option<&str>) -> String {
        match suffix {
            Some(s) => format!("{digest}--{timestamp}.{s}"),
            None => format!("{digest}--{timestamp}"),
        }
    }

    #[test]
    fn object_file_name_round_trips_all_combinations() {
        let timestamps = ["20260722T130016Z", "20260722T130016.139Z"];
        let suffixes: [Option<&str>; 3] = [None, Some("txt"), Some("tar.gz")];
        for timestamp in timestamps {
            for suffix in suffixes {
                let raw = make_object_file_name(sample_digest(), timestamp, suffix);
                let parsed = ObjectFileName::parse(&raw).unwrap();
                assert_eq!(parsed.to_file_name(), raw, "round trip for {raw:?}");
                assert_eq!(parsed.digest().as_str(), sample_digest());
                assert_eq!(parsed.timestamp().format(), timestamp);
                assert_eq!(parsed.suffix().map(|s| s.as_str()), suffix);
            }
        }
    }

    #[test]
    fn object_file_name_rejects_missing_separator() {
        let raw = format!("{}Z20260722T130016Z", sample_digest());
        assert!(ObjectFileName::parse(&raw).is_err());
    }

    #[test]
    fn object_file_name_rejects_malformed_digest() {
        let raw = format!("{}--20260722T130016Z", "z".repeat(64));
        assert!(ObjectFileName::parse(&raw).is_err());
    }

    #[test]
    fn object_file_name_rejects_missing_z() {
        let raw = format!("{}--20260722T130016", sample_digest());
        assert!(ObjectFileName::parse(&raw).is_err());
    }

    #[test]
    fn root_relative_path_from_root_strips_prefix() {
        let root = Path::new("/data/root");
        let abs = Path::new("/data/root/sub/file.txt");
        let rel = RootRelativePath::from_root(root, abs).unwrap();
        assert_eq!(rel.as_path(), Path::new("sub/file.txt"));
    }

    #[test]
    fn root_relative_path_rejects_root_itself() {
        let root = Path::new("/data/root");
        assert!(RootRelativePath::from_root(root, root).is_err());
    }

    #[test]
    fn root_relative_path_rejects_outside_root() {
        let root = Path::new("/data/root");
        let abs = Path::new("/data/other/file.txt");
        assert!(RootRelativePath::from_root(root, abs).is_err());
    }

    #[test]
    fn root_relative_path_rejects_parent_escape() {
        assert!(RootRelativePath::from_relative(Path::new("../escape")).is_err());
    }

    #[test]
    fn root_relative_path_rejects_absolute() {
        assert!(RootRelativePath::from_relative(Path::new("/absolute")).is_err());
    }
}
