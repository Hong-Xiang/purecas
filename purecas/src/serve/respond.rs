//! The one shared file-response engine used by both the visible-hierarchy
//! and digest routes: RFC 9110/9111 conditional-request evaluation
//! (delegated to [`crate::serve::precondition`]), then RFC 9110 §14 range
//! selection (using [`crate::serve::range`]'s hand-written grammar) and
//! response construction, all driven from one already-opened file
//! descriptor and one [`Representation`].
//!
//! A `304`/`412` from precondition evaluation short-circuits before any
//! `Range`/`If-Range` header is even read. Otherwise: no `Range`, an
//! unsupported range unit, or an `If-Range` validator mismatch all serve
//! the full representation; a syntactically valid `bytes` range set is
//! normalized and served as a single `206` or a `multipart/byteranges`
//! `206`, and an unsatisfiable or excessive set is `416`. `HEAD` mirrors
//! every one of these statuses and headers with an empty body.

use crate::serve::precondition::{self, Outcome as PreconditionOutcome};
use crate::serve::range::{self, ByteRange, MalformedRange, RangeHeader, Select};
use crate::serve::representation::Representation;
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use headers::{HeaderMapExt, IfRange, LastModified};
use http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use mime_guess::Mime;
use sha2::{Digest as _, Sha256};
use std::io::SeekFrom;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Bound on how many bytes a single multipart body-part read pulls into
/// memory at once, so an individual range (however large) is streamed
/// rather than buffered whole.
const MULTIPART_CHUNK_SIZE: u64 = 64 * 1024;

/// Serve `file` (opened exactly once, with `repr` already derived from
/// that same descriptor) for one `GET`/`HEAD` request.
pub async fn respond(
    method: &Method,
    headers: &HeaderMap,
    repr: &Representation,
    file: tokio::fs::File,
) -> Response {
    match precondition::evaluate(headers, method, &repr.etag, repr.last_modified) {
        Err(_) => return bad_request(),
        Ok(PreconditionOutcome::PreconditionFailed) => {
            return validators_only_response(StatusCode::PRECONDITION_FAILED, repr)
        }
        Ok(PreconditionOutcome::NotModified) => {
            return validators_only_response(StatusCode::NOT_MODIFIED, repr)
        }
        Ok(PreconditionOutcome::Proceed) => {}
    }

    let range_header = match range::read_range_header(headers) {
        Ok(r) => r,
        Err(MalformedRange) => return bad_request(),
    };

    let raw = match range_header {
        RangeHeader::Absent | RangeHeader::UnsupportedUnit => {
            return full_response(method, repr, file).await
        }
        RangeHeader::Bytes(raw) => raw,
    };

    match headers.typed_try_get::<IfRange>() {
        Err(_) => return bad_request(),
        Ok(Some(if_range)) => {
            let last_modified = LastModified::from(repr.last_modified);
            if if_range.is_modified(Some(&repr.etag), Some(&last_modified)) {
                // The representation changed (or a weak/mismatched
                // validator can never satisfy If-Range): ignore Range and
                // serve the full representation instead.
                return full_response(method, repr, file).await;
            }
        }
        Ok(None) => {}
    }

    match range::select(&raw, repr.len) {
        Select::Unsatisfiable => range_not_satisfiable_response(repr),
        Select::Ranges(ranges) if ranges.len() == 1 => {
            single_range_response(method, repr, file, ranges[0]).await
        }
        Select::Ranges(ranges) => multipart_response(method, repr, file, ranges).await,
    }
}

fn len_header_value(n: u64) -> HeaderValue {
    HeaderValue::from_str(&n.to_string()).expect("decimal length is a valid header value")
}

fn mime_header_value(mime: &Mime) -> HeaderValue {
    HeaderValue::from_str(mime.as_ref()).expect("mime_guess never returns invalid header syntax")
}

/// `ETag`, `Last-Modified`, `Cache-Control`, and `Accept-Ranges`: the
/// validators and cache/range policy shared by every representation
/// response (full, single-range, and multipart).
fn base_headers(repr: &Representation) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.typed_insert(repr.etag.clone());
    headers.typed_insert(LastModified::from(repr.last_modified));
    headers.insert(CACHE_CONTROL, repr.cache_policy.header_value());
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers
}

fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn bad_request() -> Response {
    (StatusCode::BAD_REQUEST, "Bad Request").into_response()
}

/// A present but unexpected I/O failure while streaming an already-opened
/// descriptor (e.g. a seek failure): never leak host paths or I/O
/// details to the client.
fn internal_error(context: &str, err: std::io::Error) -> Response {
    eprintln!("pcas serve: internal error: {context}: {err}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

/// Build a `304`/`412` response carrying only the representation
/// validators and an empty body.
fn validators_only_response(status: StatusCode, repr: &Representation) -> Response {
    let mut headers = HeaderMap::new();
    headers.typed_insert(repr.etag.clone());
    headers.typed_insert(LastModified::from(repr.last_modified));
    headers.insert(CACHE_CONTROL, repr.cache_policy.header_value());
    build_response(status, headers, Body::empty())
}

async fn full_response(method: &Method, repr: &Representation, file: tokio::fs::File) -> Response {
    let mut headers = base_headers(repr);
    headers.insert(CONTENT_TYPE, mime_header_value(&repr.mime));
    headers.insert(CONTENT_LENGTH, len_header_value(repr.len));

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file))
    };
    build_response(StatusCode::OK, headers, body)
}

async fn single_range_response(
    method: &Method,
    repr: &Representation,
    mut file: tokio::fs::File,
    range: ByteRange,
) -> Response {
    let mut headers = base_headers(repr);
    headers.insert(CONTENT_TYPE, mime_header_value(&repr.mime));
    headers.insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {}-{}/{}", range.start, range.end, repr.len))
            .expect("decimal Content-Range is a valid header value"),
    );
    headers.insert(CONTENT_LENGTH, len_header_value(range.byte_len()));

    if *method == Method::HEAD {
        return build_response(StatusCode::PARTIAL_CONTENT, headers, Body::empty());
    }

    if let Err(e) = file.seek(SeekFrom::Start(range.start)).await {
        return internal_error("seeking to range start", e);
    }
    let body = Body::from_stream(ReaderStream::new(file.take(range.byte_len())));
    build_response(StatusCode::PARTIAL_CONTENT, headers, body)
}

fn range_not_satisfiable_response(repr: &Representation) -> Response {
    let mut headers = base_headers(repr);
    headers.insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{}", repr.len))
            .expect("decimal Content-Range is a valid header value"),
    );
    build_response(StatusCode::RANGE_NOT_SATISFIABLE, headers, Body::empty())
}

/// A deterministic, ASCII-safe multipart boundary derived from the
/// representation's content identity (the digest, or the hierarchy
/// weak-`ETag` identity components). A boundary chosen this way could in
/// principle collide with bytes inside the content it delimits; that risk
/// is accepted because it requires an adversarial self-referential
/// collision against immutable, content-addressed data, which is
/// impractical.
fn boundary_for(identity: &str) -> String {
    let digest = Sha256::digest(identity.as_bytes());
    let mut boundary = String::with_capacity("pcasr".len() + 16);
    boundary.push_str("pcasr");
    for byte in &digest[..8] {
        boundary.push_str(&format!("{byte:02x}"));
    }
    boundary
}

/// One part's exact pre-rendered header bytes, plus the file range its
/// data comes from. `Content-Length` for the whole multipart body is
/// computed from these same header bytes, so it can never drift from
/// what the body stream actually emits.
struct PartPlan {
    header: Bytes,
    start: u64,
    end: u64,
}

impl PartPlan {
    fn byte_len(&self) -> u64 {
        self.end - self.start + 1
    }
}

fn render_part_header(boundary: &str, mime: &Mime, range: ByteRange, full_len: u64) -> Bytes {
    Bytes::from(format!(
        "--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: bytes {}-{}/{full_len}\r\n\r\n",
        range.start, range.end
    ))
}

async fn multipart_response(
    method: &Method,
    repr: &Representation,
    file: tokio::fs::File,
    ranges: Vec<ByteRange>,
) -> Response {
    let boundary = boundary_for(&repr.identity);
    let parts: Vec<PartPlan> = ranges
        .iter()
        .map(|r| PartPlan {
            header: render_part_header(&boundary, &repr.mime, *r, repr.len),
            start: r.start,
            end: r.end,
        })
        .collect();
    let final_boundary = Bytes::from(format!("--{boundary}--\r\n"));

    // Every part contributes its exact header bytes, its data bytes, and
    // a trailing CRLF; the whole body ends with the final boundary line.
    let content_length: u64 = parts
        .iter()
        .map(|p| p.header.len() as u64 + p.byte_len() + 2)
        .sum::<u64>()
        + final_boundary.len() as u64;

    let mut headers = base_headers(repr);
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}"))
            .expect("boundary is a deterministic ASCII-safe token"),
    );
    headers.insert(CONTENT_LENGTH, len_header_value(content_length));

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(multipart_stream(file, parts, final_boundary))
    };
    build_response(StatusCode::PARTIAL_CONTENT, headers, body)
}

enum Step {
    /// About to emit the next part's header, or (if there is none) the
    /// final boundary.
    NextPart,
    /// Streaming one part's data, `CHUNK`-sized bytes at a time, seeking
    /// to `next_offset` on each read.
    PartBody { end: u64, next_offset: u64 },
    /// About to emit the CRLF that ends one part's data line.
    PartTrailer,
    /// The stream is finished.
    Done,
}

struct MultipartState {
    file: tokio::fs::File,
    parts: std::collections::VecDeque<PartPlan>,
    final_boundary: Bytes,
    step: Step,
}

/// Stream a multipart/byteranges body strictly sequentially over the one
/// already-opened `file`: for each part in order, emit its pre-rendered
/// header, seek and emit its bounded data (chunked, never the whole
/// range at once), then its trailing CRLF; finish with the final
/// boundary. Never buffers the full file.
fn multipart_stream(
    file: tokio::fs::File,
    parts: Vec<PartPlan>,
    final_boundary: Bytes,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> {
    let state = MultipartState {
        file,
        parts: parts.into(),
        final_boundary,
        step: Step::NextPart,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            match std::mem::replace(&mut st.step, Step::Done) {
                Step::NextPart => match st.parts.pop_front() {
                    Some(part) => {
                        st.step = Step::PartBody {
                            end: part.end,
                            next_offset: part.start,
                        };
                        return Some((Ok(part.header), st));
                    }
                    None => {
                        // `step` is already `Done`; the next call
                        // returns `None` and the stream ends.
                        let boundary = st.final_boundary.clone();
                        return Some((Ok(boundary), st));
                    }
                },
                Step::PartBody { end, next_offset } => {
                    if next_offset > end {
                        st.step = Step::PartTrailer;
                        continue;
                    }
                    let want = (end - next_offset + 1).min(MULTIPART_CHUNK_SIZE) as usize;
                    if let Err(e) = st.file.seek(SeekFrom::Start(next_offset)).await {
                        return Some((Err(e), st));
                    }
                    let mut buf = vec![0u8; want];
                    if let Err(e) = st.file.read_exact(&mut buf).await {
                        return Some((Err(e), st));
                    }
                    st.step = Step::PartBody {
                        end,
                        next_offset: next_offset + want as u64,
                    };
                    return Some((Ok(Bytes::from(buf)), st));
                }
                Step::PartTrailer => {
                    st.step = Step::NextPart;
                    return Some((Ok(Bytes::from_static(b"\r\n")), st));
                }
                Step::Done => return None,
            }
        }
    })
}
