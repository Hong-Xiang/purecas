//! The `/pcas/<64-hex-digest>` immutable object route.
//!
//! The raw request path is matched exactly, before any percent-decoding
//! or visible-path parsing (see [`crate::serve::path`]): every other
//! top-level `/pcas` form (bare, trailing slash, extra segments,
//! percent-encoded) is left to fall through to the ordinary visible-path
//! dispatch, which already rejects the reserved `pcas` top-level segment
//! with `404`.
//!
//! Resolution reuses [`crate::index::resolve_digest`], the same typed
//! lookup `pcas path` uses, then opens the resolved object exactly once
//! and derives representation metadata and bytes from that descriptor,
//! same as the visible hierarchy.

use crate::index::{resolve_digest, DigestResolutionError};
use crate::serve::representation::Representation;
use crate::serve::respond;
use anyhow::{Context, Result};
use axum::response::Response;
use http::{HeaderMap, Method};
use std::path::Path;

/// Match the exact raw `/pcas/<digest>` request path: no trailing slash,
/// no additional segment, exactly 64 ASCII hex characters (either case).
/// Returns the still-unnormalized hex text on a match.
pub fn match_route(raw_uri_path: &str) -> Option<&str> {
    let rest = raw_uri_path.strip_prefix("/pcas/")?;
    (rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit())).then_some(rest)
}

/// Resolve and serve one digest request. `Ok` covers every outcome the
/// client should see directly (including a typed-`NotFound` `404`);
/// `Err` is unexpected I/O or packed-index corruption, left for the
/// caller to log and map to `500` without leaking host paths.
pub async fn dispatch(
    root: &Path,
    method: &Method,
    headers: &HeaderMap,
    raw_digest: &str,
) -> Result<Response> {
    let root = root.to_path_buf();
    let raw_digest = raw_digest.to_string();
    let resolution = tokio::task::spawn_blocking(move || resolve_digest(&root, &raw_digest))
        .await
        .context("digest resolution task panicked")?;

    let resolved = match resolution {
        Ok(resolved) => resolved,
        Err(DigestResolutionError::NotFound(_)) => return Ok(not_found()),
        Err(DigestResolutionError::CorruptIndex(e)) => {
            return Err(e.context("corrupt packed index resolving digest"))
        }
        Err(DigestResolutionError::Io(e)) => return Err(e.context("resolving digest")),
    };

    let file = open_resolved_object(&resolved.path).await?;
    let meta = file
        .metadata()
        .await
        .context("statting an already-opened object descriptor")?;

    let repr = Representation::for_digest(&meta, &resolved.digest, resolved.suffix.as_ref());
    Ok(respond::respond(method, headers, &repr, file).await)
}

async fn open_resolved_object(path: &Path) -> Result<tokio::fs::File> {
    tokio::fs::File::open(path)
        .await
        .context("opening a resolved digest object")
}

fn not_found() -> Response {
    use axum::response::IntoResponse;
    use http::StatusCode;
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_lowercase_digest() {
        let digest = "a".repeat(64);
        assert_eq!(
            match_route(&format!("/pcas/{digest}")),
            Some(digest.as_str())
        );
    }

    #[test]
    fn matches_uppercase_digest() {
        let digest = "A".repeat(64);
        assert_eq!(
            match_route(&format!("/pcas/{digest}")),
            Some(digest.as_str())
        );
    }

    #[test]
    fn rejects_bare_pcas() {
        assert_eq!(match_route("/pcas"), None);
    }

    #[test]
    fn rejects_trailing_slash() {
        assert_eq!(match_route(&format!("/pcas/{}/", "a".repeat(64))), None);
    }

    #[test]
    fn rejects_extra_segment() {
        assert_eq!(
            match_route(&format!("/pcas/{}/extra", "a".repeat(64))),
            None
        );
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(match_route("/pcas/deadbeef"), None);
    }

    #[test]
    fn rejects_non_hex_characters_of_correct_length() {
        let bad = format!("z{}", "a".repeat(63));
        assert_eq!(match_route(&format!("/pcas/{bad}")), None);
    }
}
