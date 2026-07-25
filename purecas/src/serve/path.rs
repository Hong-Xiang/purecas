//! Strict, exactly-once decoding of the raw percent-encoded request path
//! into raw Unix filesystem bytes.
//!
//! [`percent_encoding::percent_decode`] is deliberately **not** used for the
//! decode direction: it implements the WHATWG "string percent-decode"
//! algorithm, which passes a malformed `%` escape through unchanged instead
//! of rejecting it. That leniency conflicts with the requirement to reject
//! malformed percent-encoding with `400`, so decoding is a small,
//! explicitly validated routine here. `percent_encoding` is still the right
//! tool for the encode direction (used by the directory-listing renderer).
//!
//! [`http::Uri::path`] is the sole decoding input: it is still in its raw
//! percent-encoded, unnormalized wire form, unlike axum's decoded `Path`
//! extractor, which must never be used for the visible hierarchy.

use percent_encoding::{percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::fmt;

const URI_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A parsed, validated visible request path.
///
/// Each element of `segments` is one `/`-delimited path segment, decoded
/// exactly once into raw bytes (never containing NUL or `/`). `pcas` and
/// `.pcas` are rejected as top-level segments before they reach this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisiblePath {
    pub segments: Vec<Vec<u8>>,
    /// Whether the raw URI path ended in `/` (including the root path `/`).
    pub trailing_slash: bool,
}

impl VisiblePath {
    /// Losslessly encode this parsed path into its canonical hierarchy URL.
    pub fn encoded_path(&self) -> String {
        let encoded = self
            .segments
            .iter()
            .map(|segment| percent_encode(segment, URI_SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/");
        format!("/{encoded}")
    }
}

/// Why a raw request path was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// A `%` was not followed by two hexadecimal digits.
    MalformedPercentEncoding,
    /// A decoded segment contained a NUL byte.
    Nul,
    /// The path can never correspond to a real filesystem entry: an empty
    /// interior segment (doubled `/`), a `..` segment, or a segment whose
    /// decoded bytes contain `/` (which must never be reinterpreted as an
    /// additional path separator).
    Invalid,
    /// The top-level segment is `pcas` (reserved for the digest route) or
    /// `.pcas` (reserved for internal state).
    ReservedTopLevel,
    /// The whole path is the top-level legacy `purecas.db`, which must
    /// never be exposed even if it exists on disk.
    LegacyDatabase,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::MalformedPercentEncoding => "malformed percent-encoding",
            Self::Nul => "NUL byte in path",
            Self::Invalid => "invalid path",
            Self::ReservedTopLevel => "reserved top-level path",
            Self::LegacyDatabase => "top-level legacy database path",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for PathError {}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode one raw (still percent-encoded) `/`-delimited segment into raw
/// bytes, exactly once.
fn decode_segment(raw: &str) -> Result<Vec<u8>, PathError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_value);
            let lo = bytes.get(i + 2).copied().and_then(hex_value);
            let (hi, lo) = match (hi, lo) {
                (Some(hi), Some(lo)) => (hi, lo),
                _ => return Err(PathError::MalformedPercentEncoding),
            };
            let decoded = (hi << 4) | lo;
            if decoded == 0 {
                return Err(PathError::Nul);
            }
            out.push(decoded);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Parse the raw percent-encoded [`http::Uri::path`] of a request.
pub fn parse(raw_uri_path: &str) -> Result<VisiblePath, PathError> {
    debug_assert!(
        raw_uri_path.starts_with('/'),
        "Uri::path always starts with /"
    );
    let body = &raw_uri_path[1..];
    let trailing_slash = body.is_empty() || body.ends_with('/');

    let mut raw_segments: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        body.split('/').collect()
    };
    if trailing_slash {
        // The trailing `/` produces one extra empty element from `split`;
        // it only signals `trailing_slash` and is never itself a segment.
        raw_segments.pop();
    }

    let mut segments = Vec::with_capacity(raw_segments.len());
    for raw in raw_segments {
        if raw.is_empty() {
            // An interior empty segment: a doubled `/`, e.g. `/a//b`. No
            // real filesystem entry can have an empty name.
            return Err(PathError::Invalid);
        }
        let decoded = decode_segment(raw)?;
        if decoded.contains(&b'/') {
            return Err(PathError::Invalid);
        }
        if decoded == b".." {
            return Err(PathError::Invalid);
        }
        segments.push(decoded);
    }

    if matches!(
        segments.first().map(Vec::as_slice),
        Some(b"pcas") | Some(b".pcas")
    ) {
        return Err(PathError::ReservedTopLevel);
    }
    if segments.len() == 1 && segments[0] == b"purecas.db" {
        return Err(PathError::LegacyDatabase);
    }

    Ok(VisiblePath {
        segments,
        trailing_slash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(v: &[&[u8]]) -> Vec<Vec<u8>> {
        v.iter().map(|s| s.to_vec()).collect()
    }

    #[test]
    fn root() {
        let p = parse("/").unwrap();
        assert_eq!(p.segments, Vec::<Vec<u8>>::new());
        assert!(p.trailing_slash);
    }

    #[test]
    fn simple_file() {
        let p = parse("/foo/bar.txt").unwrap();
        assert_eq!(p.segments, segs(&[b"foo", b"bar.txt"]));
        assert!(!p.trailing_slash);
    }

    #[test]
    fn directory_with_slash() {
        let p = parse("/foo/").unwrap();
        assert_eq!(p.segments, segs(&[b"foo"]));
        assert!(p.trailing_slash);
    }

    #[test]
    fn percent_decodes_once() {
        let p = parse("/a%20b/%e4%bd%a0").unwrap();
        assert_eq!(p.segments[0], b"a b");
        assert_eq!(p.segments[1], "你".as_bytes());
    }

    #[test]
    fn canonical_encoding_escapes_every_non_unreserved_byte() {
        let p = parse("/a%5cb%5ec/%ff").unwrap();
        assert_eq!(p.encoded_path(), "/a%5Cb%5Ec/%FF");
    }

    #[test]
    fn malformed_percent_short() {
        assert_eq!(parse("/a%2"), Err(PathError::MalformedPercentEncoding));
    }

    #[test]
    fn malformed_percent_non_hex() {
        assert_eq!(parse("/a%zz"), Err(PathError::MalformedPercentEncoding));
    }

    #[test]
    fn nul_byte_rejected() {
        assert_eq!(parse("/a%00b"), Err(PathError::Nul));
    }

    #[test]
    fn encoded_dot_dot_traversal_rejected() {
        assert_eq!(parse("/a/%2e%2e/b"), Err(PathError::Invalid));
        assert_eq!(parse("/.."), Err(PathError::Invalid));
    }

    #[test]
    fn encoded_slash_edge_case_rejected() {
        // `%2F` decodes to a raw `/` byte *within* one segment; it must
        // never be reinterpreted as an extra path separator, and no real
        // single filesystem entry can contain it.
        assert_eq!(parse("/foo%2Fbar"), Err(PathError::Invalid));
    }

    #[test]
    fn doubled_slash_rejected() {
        assert_eq!(parse("/foo//bar"), Err(PathError::Invalid));
    }

    #[test]
    fn reserved_top_level_pcas() {
        assert_eq!(parse("/pcas"), Err(PathError::ReservedTopLevel));
        assert_eq!(parse("/pcas/deadbeef"), Err(PathError::ReservedTopLevel));
        // Percent-encoded spelling of the reserved segment is still caught,
        // because reservation is checked against decoded bytes.
        assert_eq!(parse("/%70cas"), Err(PathError::ReservedTopLevel));
    }

    #[test]
    fn reserved_top_level_dot_pcas() {
        assert_eq!(parse("/.pcas"), Err(PathError::ReservedTopLevel));
        assert_eq!(
            parse("/.pcas/sha256/object"),
            Err(PathError::ReservedTopLevel)
        );
        assert_eq!(
            parse("/%2epcas/index.lock"),
            Err(PathError::ReservedTopLevel)
        );
    }

    #[test]
    fn nested_pcas_is_not_reserved() {
        let p = parse("/foo/pcas/bar").unwrap();
        assert_eq!(p.segments, segs(&[b"foo", b"pcas", b"bar"]));
    }

    #[test]
    fn nested_dot_pcas_is_not_reserved() {
        let p = parse("/foo/.pcas/bar").unwrap();
        assert_eq!(p.segments, segs(&[b"foo", b".pcas", b"bar"]));
    }

    #[test]
    fn top_level_legacy_database_rejected() {
        assert_eq!(parse("/purecas.db"), Err(PathError::LegacyDatabase));
    }

    #[test]
    fn nested_legacy_database_name_is_not_reserved() {
        let p = parse("/foo/purecas.db").unwrap();
        assert_eq!(p.segments, segs(&[b"foo", b"purecas.db"]));
    }
}
