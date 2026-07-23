//! RFC 9110 §13.2.2 conditional-request evaluation, shared by the visible
//! file responder and (once it lands) the digest route.
//!
//! Header parsing and comparison semantics (strong vs. weak `ETag`
//! comparison, wildcard handling, `HTTP-date` parsing) are delegated to the
//! maintained `headers` crate rather than reimplemented: `IfMatch` performs
//! strong comparison (so a weak `ETag` — which this server always
//! generates — can only ever satisfy `If-Match: *`, per RFC 9110 §13.1.1),
//! while `IfNoneMatch` performs weak comparison, matching the RFC exactly.
//!
//! `typed_try_get` surfaces a [`MalformedCondition`] error (mapped to `400`
//! by the caller) whenever a present header cannot be parsed at all, most
//! notably a garbage `If-Modified-Since`/`If-Unmodified-Since` `HTTP-date`.
//! An entity-tag *list* (`If-Match`/`If-None-Match`) is validated per-tag
//! only when comparing, so an individual unparseable tag is never silently
//! treated as an absent header either — it simply can never match, which
//! safely fails the request closed (`412`) rather than serving a cached or
//! stale representation.

use headers::{ETag, HeaderMapExt, IfMatch, IfModifiedSince, IfNoneMatch, IfUnmodifiedSince};
use http::{HeaderMap, Method};
use std::time::SystemTime;

/// The result of evaluating all applicable preconditions, in RFC 9110
/// order, against one representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No precondition blocked the request; proceed to construct the
    /// normal (200) response.
    Proceed,
    /// `If-None-Match` or `If-Modified-Since` blocked the request: `304`.
    NotModified,
    /// `If-Match` or `If-Unmodified-Since` blocked the request: `412`.
    PreconditionFailed,
}

/// A present conditional-request header failed to parse. The caller must
/// not silently treat this as an absent header; the request should be
/// rejected rather than the precondition ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedCondition;

/// Evaluate `If-Match`, `If-Unmodified-Since`, `If-None-Match`, and
/// `If-Modified-Since` (the last only for `GET`/`HEAD`) in RFC 9110 order.
pub fn evaluate(
    headers: &HeaderMap,
    method: &Method,
    etag: &ETag,
    last_modified: SystemTime,
) -> Result<Outcome, MalformedCondition> {
    let if_match = headers
        .typed_try_get::<IfMatch>()
        .map_err(|_| MalformedCondition)?;
    if let Some(if_match) = if_match {
        if !if_match.precondition_passes(etag) {
            return Ok(Outcome::PreconditionFailed);
        }
    } else {
        let if_unmodified_since = headers
            .typed_try_get::<IfUnmodifiedSince>()
            .map_err(|_| MalformedCondition)?;
        if let Some(if_unmodified_since) = if_unmodified_since {
            if !if_unmodified_since.precondition_passes(last_modified) {
                return Ok(Outcome::PreconditionFailed);
            }
        }
    }

    let if_none_match = headers
        .typed_try_get::<IfNoneMatch>()
        .map_err(|_| MalformedCondition)?;
    if let Some(if_none_match) = if_none_match {
        if !if_none_match.precondition_passes(etag) {
            return Ok(Outcome::NotModified);
        }
    } else if matches!(*method, Method::GET | Method::HEAD) {
        let if_modified_since = headers
            .typed_try_get::<IfModifiedSince>()
            .map_err(|_| MalformedCondition)?;
        if let Some(if_modified_since) = if_modified_since {
            if !if_modified_since.is_modified(last_modified) {
                return Ok(Outcome::NotModified);
            }
        }
    }

    Ok(Outcome::Proceed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use std::time::Duration;

    fn etag(s: &str) -> ETag {
        s.parse().unwrap()
    }

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn no_preconditions_proceeds() {
        let h = HeaderMap::new();
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now()).unwrap();
        assert_eq!(out, Outcome::Proceed);
    }

    #[test]
    fn if_match_wildcard_passes() {
        let h = headers_with("if-match", "*");
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now()).unwrap();
        assert_eq!(out, Outcome::Proceed);
    }

    #[test]
    fn if_match_weak_etag_never_strong_matches() {
        // A weak ETag can never satisfy a specific If-Match per RFC 9110;
        // only `If-Match: *` can.
        let h = headers_with("if-match", "\"a-b-1-2\"");
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now()).unwrap();
        assert_eq!(out, Outcome::PreconditionFailed);
    }

    #[test]
    fn if_unmodified_since_skipped_when_if_match_present() {
        let now = SystemTime::now();
        let mut h = headers_with("if-match", "*");
        h.typed_insert(IfUnmodifiedSince::from(now - Duration::from_secs(3600)));
        // If-Match: * passes, so If-Unmodified-Since must never be
        // consulted even though it would fail on its own.
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), now).unwrap();
        assert_eq!(out, Outcome::Proceed);
    }

    #[test]
    fn if_unmodified_since_fails_when_stale() {
        let now = SystemTime::now();
        let mut h = HeaderMap::new();
        h.typed_insert(IfUnmodifiedSince::from(now - Duration::from_secs(3600)));
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), now).unwrap();
        assert_eq!(out, Outcome::PreconditionFailed);
    }

    #[test]
    fn if_none_match_weak_comparison() {
        let h = headers_with("if-none-match", "\"a-b-1-2\"");
        // Weak comparison: "a-b-1-2" matches W/"a-b-1-2".
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now()).unwrap();
        assert_eq!(out, Outcome::NotModified);
    }

    #[test]
    fn if_none_match_wildcard_not_modified() {
        let h = headers_with("if-none-match", "*");
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now()).unwrap();
        assert_eq!(out, Outcome::NotModified);
    }

    #[test]
    fn if_modified_since_skipped_when_if_none_match_present() {
        let now = SystemTime::now();
        let mut h = headers_with("if-none-match", "\"different\"");
        h.typed_insert(IfModifiedSince::from(now + Duration::from_secs(3600)));
        // If-None-Match doesn't match (proceeds), so If-Modified-Since must
        // never be consulted even though it would say "not modified".
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), now).unwrap();
        assert_eq!(out, Outcome::Proceed);
    }

    #[test]
    fn if_modified_since_not_modified() {
        let now = SystemTime::now();
        let mut h = HeaderMap::new();
        h.typed_insert(IfModifiedSince::from(now + Duration::from_secs(3600)));
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), now).unwrap();
        assert_eq!(out, Outcome::NotModified);
    }

    #[test]
    fn if_modified_since_ignored_for_other_methods() {
        // Only meaningful for GET/HEAD; this module is only ever invoked
        // for GET/HEAD in this slice, but the guard is tested directly.
        let now = SystemTime::now();
        let mut h = HeaderMap::new();
        h.typed_insert(IfModifiedSince::from(now + Duration::from_secs(3600)));
        let out = evaluate(&h, &Method::POST, &etag("W/\"a-b-1-2\""), now).unwrap();
        assert_eq!(out, Outcome::Proceed);
    }

    #[test]
    fn malformed_if_modified_since_is_rejected_not_ignored() {
        // Unlike an entity-tag list (where the underlying crate lazily
        // treats any unparseable individual tag as simply "no match",
        // which still safely fails closed), an `HTTP-date` is strictly
        // parsed: a garbage value must be rejected rather than silently
        // treated as an absent header.
        let h = headers_with("if-modified-since", "not-a-date");
        let err = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now());
        assert_eq!(err, Err(MalformedCondition));
    }

    #[test]
    fn syntactically_invalid_etag_list_fails_the_precondition_rather_than_erroring() {
        // The `headers` crate validates individual entity-tags lazily when
        // comparing, not when parsing the list; an unparseable tag simply
        // never matches, which for `If-Match` still safely denies the
        // request (412) rather than silently proceeding.
        let h = headers_with("if-match", "not-a-valid-etag");
        let out = evaluate(&h, &Method::GET, &etag("W/\"a-b-1-2\""), SystemTime::now()).unwrap();
        assert_eq!(out, Outcome::PreconditionFailed);
    }
}
