//! The visible-hierarchy axum router: request dispatch, GET/HEAD file and
//! directory responses, and error-status mapping. Built and testable
//! without opening a network socket (see `router()`); `listen()` is the
//! only function that actually binds a socket.

use crate::serve::listing;
use crate::serve::path::{self, PathError, VisiblePath};
use crate::serve::precondition::{self, Outcome as PreconditionOutcome};
use crate::serve::representation::Representation;
use crate::serve::resolve::{self, Resolved, Root};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::Router;
use headers::{CacheControl, HeaderMapExt, LastModified};
use http::header::{ACCEPT_RANGES, ALLOW, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use tokio_util::io::ReaderStream;

/// Shared, immutable server state.
pub struct AppState {
    root: Root,
}

impl AppState {
    pub fn new(root: Root) -> Self {
        Self { root }
    }
}

/// Build the router. No network socket is opened; suitable for direct
/// in-process request dispatch in tests.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new().fallback(handle).with_state(state)
}

async fn handle(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().clone();
    if !matches!(method, Method::GET | Method::HEAD) {
        return method_not_allowed();
    }

    let raw_path = req.uri().path();
    let parsed = match path::parse(raw_path) {
        Ok(parsed) => parsed,
        Err(PathError::MalformedPercentEncoding | PathError::Nul) => return bad_request(),
        Err(PathError::Invalid | PathError::ReservedTopLevel | PathError::LegacyDatabase) => {
            return not_found()
        }
    };

    match dispatch(&state, &method, req.uri(), req.headers(), &parsed).await {
        Ok(response) => response,
        Err(err) => {
            // Never leak host paths or I/O details to the client.
            log_internal_error(&err);
            internal_error()
        }
    }
}

/// Log without pulling in a tracing dependency: this slice keeps its
/// footprint minimal, so an internal-error diagnostic goes to stderr only.
fn log_internal_error(err: &anyhow::Error) {
    eprintln!("pcas serve: internal error: {err:#}");
}

async fn dispatch(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    parsed: &VisiblePath,
) -> Result<Response> {
    let resolved = resolve::resolve(&state.root, &parsed.segments).await?;
    match resolved {
        Resolved::NotFound => Ok(not_found()),
        Resolved::File(opened) => {
            if parsed.trailing_slash {
                // "/visible/file.txt/" cannot name the same resource as
                // "/visible/file.txt"; only directories legitimately end in
                // `/` in this scheme.
                return Ok(not_found());
            }
            let requested_name = parsed.segments.last().map(Vec::as_slice).unwrap_or(b"");
            Ok(respond_file(
                method,
                headers,
                requested_name,
                opened.file,
                opened.meta,
            ))
        }
        Resolved::Directory { canonical_path } => {
            respond_directory(state, method, uri, &canonical_path, parsed.trailing_slash).await
        }
    }
}

fn respond_file(
    method: &Method,
    headers: &HeaderMap,
    requested_name: &[u8],
    file: tokio::fs::File,
    meta: std::fs::Metadata,
) -> Response {
    let repr = Representation::from_metadata(&meta);

    match precondition::evaluate(headers, method, &repr.etag, repr.last_modified) {
        Err(_) => return bad_request(),
        Ok(PreconditionOutcome::PreconditionFailed) => {
            return representation_only_response(StatusCode::PRECONDITION_FAILED, &repr)
        }
        Ok(PreconditionOutcome::NotModified) => {
            return representation_only_response(StatusCode::NOT_MODIFIED, &repr)
        }
        Ok(PreconditionOutcome::Proceed) => {}
    }

    let mime =
        mime_guess::from_path(Path::new(OsStr::from_bytes(requested_name))).first_or_octet_stream();

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&repr.len.to_string())
            .expect("decimal length is a valid header value"),
    );
    response_headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .expect("mime_guess never returns invalid header syntax"),
    );
    response_headers.typed_insert(LastModified::from(repr.last_modified));
    response_headers.typed_insert(repr.etag.clone());
    response_headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.typed_insert(CacheControl::new().with_no_cache());

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file))
    };

    let mut response = Response::new(body);
    *response.headers_mut() = response_headers;
    response
}

/// Build a `304`/`412` response carrying only the representation
/// validators (`ETag`, `Last-Modified`, `Cache-Control`) and an empty body.
fn representation_only_response(status: StatusCode, repr: &Representation) -> Response {
    let mut headers = HeaderMap::new();
    headers.typed_insert(repr.etag.clone());
    headers.typed_insert(LastModified::from(repr.last_modified));
    headers.typed_insert(CacheControl::new().with_no_cache());
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

async fn respond_directory(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    canonical_dir: &Path,
    trailing_slash: bool,
) -> Result<Response> {
    if !trailing_slash {
        return Ok(redirect_with_slash(uri));
    }

    if let Resolved::File(opened) =
        resolve::resolve_child(&state.root, canonical_dir, b"index.html").await?
    {
        let empty_headers = HeaderMap::new();
        return Ok(respond_file(
            method,
            &empty_headers,
            b"index.html",
            opened.file,
            opened.meta,
        ));
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(canonical_dir)
        .await
        .context("reading directory")?;
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .context("reading directory entry")?
    {
        let name = entry.file_name();
        let name_bytes = name.as_bytes();
        if name_bytes == b".pcas" {
            continue;
        }
        if canonical_dir == state.root.canonical_root() && name_bytes == b"purecas.db" {
            continue;
        }
        // Best-effort: a directory listing is inherently advisory, so a
        // single entry whose type can't be determined (e.g. a dangling
        // symlink) is rendered as a plain (non-directory) link rather than
        // failing the whole listing.
        let is_dir = tokio::fs::metadata(entry.path())
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        entries.push(listing::Entry {
            raw_name: name_bytes.to_vec(),
            is_dir,
        });
    }

    let show_parent_link = canonical_dir != state.root.canonical_root();
    let html = listing::render(entries, show_parent_link);

    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&html.len().to_string())
            .expect("decimal length is a valid header value"),
    );
    headers.typed_insert(CacheControl::new().with_no_cache());

    let body = if *method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(html)
    };
    let mut response = Response::new(body);
    *response.headers_mut() = headers;
    Ok(response)
}

fn redirect_with_slash(uri: &Uri) -> Response {
    let mut location = format!("{}/", uri.path());
    if let Some(query) = uri.query() {
        location.push('?');
        location.push_str(query);
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::MOVED_PERMANENTLY;
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    response
}

fn bad_request() -> Response {
    (StatusCode::BAD_REQUEST, "Bad Request").into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

fn method_not_allowed() -> Response {
    let mut response = (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response();
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn make_router(root: &Path) -> Router {
        let root = Root::open(root).unwrap();
        router(Arc::new(AppState::new(root)))
    }

    async fn request(
        router: Router,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(Body::empty()).unwrap();
        router.oneshot(req).await.unwrap()
    }

    async fn get(router: Router, uri: &str) -> Response {
        request(router, Method::GET, uri, &[]).await
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[tokio::test]
    async fn nested_file_streams_exact_bytes_and_headers() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), b"hello world").unwrap();

        let response = get(make_router(dir.path()), "/a/b/c.txt").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "content-length"), Some("11"));
        assert_eq!(header(&response, "content-type"), Some("text/plain"));
        assert_eq!(header(&response, "accept-ranges"), Some("bytes"));
        assert_eq!(header(&response, "cache-control"), Some("no-cache"));
        assert!(header(&response, "last-modified").is_some());
        let etag = header(&response, "etag").unwrap().to_string();
        assert!(etag.starts_with("W/\""), "{etag}");
        assert_eq!(etag.split('-').count(), 4, "{etag}");

        let body = body_bytes(response).await;
        assert_eq!(body, b"hello world");
    }

    #[tokio::test]
    async fn head_has_identical_representation_headers_and_empty_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();

        let get_response = get(make_router(dir.path()), "/f.txt").await;
        let head_response = request(make_router(dir.path()), Method::HEAD, "/f.txt", &[]).await;

        assert_eq!(head_response.status(), StatusCode::OK);
        for name in [
            "content-length",
            "content-type",
            "last-modified",
            "etag",
            "accept-ranges",
            "cache-control",
        ] {
            assert_eq!(
                header(&get_response, name),
                header(&head_response, name),
                "header {name} differs"
            );
        }
        assert!(body_bytes(head_response).await.is_empty());
    }

    #[tokio::test]
    async fn if_match_wildcard_proceeds() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[("if-match", "*")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn if_match_specific_etag_returns_412_with_validators() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[("if-match", "\"does-not-match\"")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert!(header(&response, "etag").is_some());
        assert!(header(&response, "last-modified").is_some());
        assert!(body_bytes(response).await.is_empty());
    }

    #[tokio::test]
    async fn if_none_match_current_etag_returns_304() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let etag = header(&get(make_router(dir.path()), "/f.txt").await, "etag")
            .unwrap()
            .to_string();

        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[("if-none-match", &etag)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(header(&response, "etag"), Some(etag.as_str()));
        assert!(body_bytes(response).await.is_empty());
    }

    #[tokio::test]
    async fn if_unmodified_since_in_the_past_returns_412() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[("if-unmodified-since", "Mon, 01 Jan 1990 00:00:00 GMT")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn if_modified_since_in_the_future_returns_304() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[("if-modified-since", "Tue, 01 Jan 2999 00:00:00 GMT")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn if_match_takes_precedence_over_if_unmodified_since() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        // If-Match: * passes, so the (otherwise failing) If-Unmodified-Since
        // must never be consulted.
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[
                ("if-match", "*"),
                ("if-unmodified-since", "Mon, 01 Jan 1990 00:00:00 GMT"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn if_none_match_takes_precedence_over_if_modified_since() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        // If-None-Match doesn't match (proceeds), so the (otherwise
        // "not modified") If-Modified-Since must never be consulted.
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.txt",
            &[
                ("if-none-match", "\"does-not-match\""),
                ("if-modified-since", "Tue, 01 Jan 2999 00:00:00 GMT"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn directory_without_trailing_slash_redirects_preserving_query() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let response = get(make_router(dir.path()), "/sub?x=1&y=2").await;
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(header(&response, "location"), Some("/sub/?x=1&y=2"));
    }

    #[tokio::test]
    async fn directory_with_index_html_gets_full_file_semantics() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/index.html"), b"<h1>hi</h1>").unwrap();
        let response = get(make_router(dir.path()), "/sub/").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "content-type"), Some("text/html"));
        assert!(header(&response, "etag").is_some());
        assert_eq!(body_bytes(response).await, b"<h1>hi</h1>");
    }

    #[tokio::test]
    async fn directory_listing_is_sorted_escaped_encoded_with_parent_link() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/child")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"").unwrap();
        std::fs::write(dir.path().join("sub/a<.txt"), b"").unwrap();

        let response = get(make_router(dir.path()), "/sub/").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, "content-type"),
            Some("text/html; charset=utf-8")
        );
        assert!(header(&response, "etag").is_none());
        assert!(header(&response, "last-modified").is_none());
        assert_eq!(header(&response, "cache-control"), Some("no-cache"));

        let html = String::from_utf8(body_bytes(response).await).unwrap();
        assert!(html.contains("href=\"../\""), "{html}");
        assert!(html.contains("href=\"child/\">child/</a>"), "{html}");
        assert!(html.contains("&#60;"), "{html}"); // `a<.txt` label is escaped
        let a_pos = html.find("a%3C.txt").unwrap();
        let b_pos = html.find("b.txt").unwrap();
        assert!(a_pos < b_pos, "expected deterministic sort order: {html}");
    }

    #[tokio::test]
    async fn root_listing_has_no_parent_link() {
        let dir = TempDir::new().unwrap();
        let html =
            String::from_utf8(body_bytes(get(make_router(dir.path()), "/").await).await).unwrap();
        assert!(!html.contains("href=\"../\""), "{html}");
    }

    #[tokio::test]
    async fn directory_listing_shows_non_utf8_names() {
        let dir = TempDir::new().unwrap();
        let raw_name = std::ffi::OsStr::from_bytes(&[b'x', 0xFF, b'y', b'.', b't', b'x', b't']);
        std::fs::write(dir.path().join(raw_name), b"").unwrap();
        let html =
            String::from_utf8(body_bytes(get(make_router(dir.path()), "/").await).await).unwrap();
        assert!(html.contains("href=\"x%FFy.txt\""), "{html}");
    }

    #[tokio::test]
    async fn head_directory_listing_has_no_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"").unwrap();
        let response = request(make_router(dir.path()), Method::HEAD, "/", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(header(&response, "content-length").is_some());
        assert!(body_bytes(response).await.is_empty());
    }

    #[tokio::test]
    async fn malformed_percent_encoding_is_400() {
        let dir = TempDir::new().unwrap();
        let response = get(make_router(dir.path()), "/a%zz").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn nul_byte_is_400() {
        let dir = TempDir::new().unwrap();
        let response = get(make_router(dir.path()), "/a%00b").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn encoded_dot_dot_traversal_is_404() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        let response = get(make_router(dir.path()), "/a/%2e%2e/etc/passwd").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn encoded_slash_edge_case_is_404() {
        let dir = TempDir::new().unwrap();
        let response = get(make_router(dir.path()), "/foo%2Fbar").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn missing_path_is_404() {
        let dir = TempDir::new().unwrap();
        let response = get(make_router(dir.path()), "/does/not/exist").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn direct_dot_pcas_is_404() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".pcas/sha256")).unwrap();
        std::fs::write(dir.path().join(".pcas/sha256/x"), b"secret").unwrap();
        assert_eq!(
            get(make_router(dir.path()), "/.pcas/sha256/x")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(make_router(dir.path()), "/.pcas/").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn symlink_to_dot_pcas_is_404() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".pcas")).unwrap();
        std::fs::write(dir.path().join(".pcas/secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join(".pcas"), dir.path().join("link")).unwrap();
        assert_eq!(
            get(make_router(dir.path()), "/link/secret").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn symlink_outside_root_is_404() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        assert_eq!(
            get(make_router(dir.path()), "/link.txt").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn inside_root_symlink_is_served() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("real.txt"), b"real content").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();
        let response = get(make_router(dir.path()), "/link.txt").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"real content");
    }

    #[tokio::test]
    async fn reserved_pcas_route_is_404() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            get(make_router(dir.path()), "/pcas").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(make_router(dir.path()), "/pcas/deadbeef")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn nested_pcas_directory_remains_visible() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("foo/pcas")).unwrap();
        std::fs::write(dir.path().join("foo/pcas/bar.txt"), b"nested pcas").unwrap();
        let response = get(make_router(dir.path()), "/foo/pcas/bar.txt").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"nested pcas");
    }

    #[tokio::test]
    async fn post_and_other_methods_are_405_with_allow_header() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        for method in [Method::POST, Method::PUT, Method::DELETE] {
            let response = request(make_router(dir.path()), method.clone(), "/f.txt", &[]).await;
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method}"
            );
            assert_eq!(header(&response, "allow"), Some("GET, HEAD"));
        }
    }

    #[tokio::test]
    async fn top_level_legacy_purecas_db_is_not_served() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("purecas.db"), b"legacy sqlite bytes").unwrap();
        assert_eq!(
            get(make_router(dir.path()), "/purecas.db").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn top_level_legacy_purecas_db_is_excluded_from_root_listing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("purecas.db"), b"legacy sqlite bytes").unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"").unwrap();
        let html =
            String::from_utf8(body_bytes(get(make_router(dir.path()), "/").await).await).unwrap();
        assert!(!html.contains("purecas.db"), "{html}");
        assert!(html.contains("visible.txt"), "{html}");
    }

    #[tokio::test]
    async fn nested_purecas_db_name_is_visible_and_listed() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/purecas.db"), b"just a file here").unwrap();
        let response = get(make_router(dir.path()), "/sub/purecas.db").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"just a file here");
    }

    #[tokio::test]
    async fn serving_never_creates_or_touches_sqlite_database() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        assert_eq!(
            get(make_router(dir.path()), "/f.txt").await.status(),
            StatusCode::OK
        );
        assert_eq!(
            get(make_router(dir.path()), "/").await.status(),
            StatusCode::OK
        );
        assert!(!dir.path().join("purecas.db").exists());
    }
}
