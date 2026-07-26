//! The visible-hierarchy axum router: request dispatch, GET/HEAD file and
//! directory responses, and error-status mapping. Built and testable
//! without opening a network socket (see `router()`); `listen()` is the
//! only function that actually binds a socket.

use crate::serve::digest;
use crate::serve::ingest::{self, IngestError};
use crate::serve::listing;
use crate::serve::path::{self, PathError, VisiblePath};
use crate::serve::process::{ProcessRoutes, RouteLookup};
use crate::serve::representation::Representation;
use crate::serve::resolve::{self, Resolved, Root};
use crate::serve::respond;
use crate::serve::IngestionMode;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::Router;
use headers::{CacheControl, HeaderMapExt};
use http::header::{ALLOW, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LOCATION, RETRY_AFTER};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use serde_json::json;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

/// Shared, immutable server state.
pub struct AppState {
    root: Root,
    ingestion: IngestionMode,
    process_routes: Option<ProcessRoutes>,
}

impl AppState {
    pub fn new(root: Root) -> Self {
        Self {
            root,
            ingestion: IngestionMode::ReadOnly,
            process_routes: None,
        }
    }

    pub fn with_ingestion(root: Root, ingestion: IngestionMode) -> Self {
        Self {
            root,
            ingestion,
            process_routes: None,
        }
    }

    pub fn with_options(
        root: Root,
        ingestion: IngestionMode,
        process_routes: Option<ProcessRoutes>,
    ) -> Self {
        Self {
            root,
            ingestion,
            process_routes,
        }
    }
}

/// Build the router. No network socket is opened; suitable for direct
/// in-process request dispatch in tests.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new().fallback(handle).with_state(state)
}

async fn handle(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();

    // The digest route is intercepted from the exact raw request path,
    // before any percent-decoding or visible-path parsing: every other
    // top-level `/pcas` form falls through to ordinary dispatch below,
    // which already rejects the reserved `pcas` segment with `404`.
    if let Some(raw_digest) = digest::match_route(&raw_path) {
        if !matches!(method, Method::GET | Method::HEAD) {
            return method_not_allowed(false);
        }
        return match digest::dispatch(
            state.root.canonical_root(),
            &method,
            req.headers(),
            raw_digest,
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                log_internal_error(&err);
                internal_error()
            }
        };
    }

    let process_path = if state.process_routes.is_some() {
        match path::parse(&raw_path) {
            Ok(parsed) => Some(parsed),
            Err(PathError::MalformedPercentEncoding | PathError::Nul) if method == Method::POST => {
                return bad_request()
            }
            Err(PathError::Invalid | PathError::ReservedTopLevel | PathError::LegacyDatabase)
                if method == Method::POST =>
            {
                return not_found()
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let process_match = match (&state.process_routes, &process_path) {
        (Some(routes), Some(path)) => routes.lookup(path),
        _ => RouteLookup::None,
    };
    if method == Method::POST {
        match process_match {
            RouteLookup::Matched(matched) => {
                return crate::serve::process::dispatch(matched, req).await
            }
            RouteLookup::InvalidCapture => return not_found(),
            RouteLookup::None => {}
        }
    }

    let is_ingest = method == Method::POST && state.ingestion == IngestionMode::Allow;
    if !matches!(method, Method::GET | Method::HEAD) && !is_ingest {
        return method_not_allowed(
            state.ingestion == IngestionMode::Allow
                || state
                    .process_routes
                    .as_ref()
                    .zip(process_path.as_ref())
                    .is_some_and(|(routes, path)| {
                        !matches!(routes.lookup(path), RouteLookup::None)
                    }),
        );
    }

    let parsed = match path::parse(&raw_path) {
        Ok(parsed) => parsed,
        Err(PathError::MalformedPercentEncoding | PathError::Nul) => return bad_request(),
        Err(PathError::Invalid | PathError::ReservedTopLevel | PathError::LegacyDatabase) => {
            return not_found()
        }
    };

    if is_ingest {
        return match ingest::ingest(&state.root, parsed, req.into_body()).await {
            Ok(indexed) => created(indexed),
            Err(IngestError::NotFound) => not_found(),
            Err(IngestError::Conflict) => conflict(),
            Err(IngestError::BadBody(error)) => {
                log_internal_error(&error);
                bad_request()
            }
            Err(IngestError::Internal(error)) => {
                log_internal_error(&error);
                internal_error()
            }
            Err(IngestError::IndexBusy(error)) => {
                log_internal_error(&error);
                index_lock_timeout()
            }
            Err(IngestError::CrossDevice(error)) => {
                log_internal_error(&error);
                cross_device()
            }
            Err(IngestError::IndexFailed(error)) => {
                log_internal_error(&error);
                internal_error()
            }
        };
    }

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
            Ok(respond_file(method, headers, requested_name, opened.file, opened.meta).await)
        }
        Resolved::Directory { canonical_path } => {
            respond_directory(
                state,
                method,
                uri,
                headers,
                &canonical_path,
                parsed.trailing_slash,
            )
            .await
        }
    }
}

async fn respond_file(
    method: &Method,
    headers: &HeaderMap,
    requested_name: &[u8],
    file: tokio::fs::File,
    meta: std::fs::Metadata,
) -> Response {
    let repr = Representation::for_hierarchy(&meta, requested_name);
    respond::respond(method, headers, &repr, file).await
}

async fn respond_directory(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    canonical_dir: &Path,
    trailing_slash: bool,
) -> Result<Response> {
    if !trailing_slash {
        return Ok(redirect_with_slash(uri));
    }

    if let Resolved::File(opened) =
        resolve::resolve_child(&state.root, canonical_dir, b"index.html").await?
    {
        return Ok(respond_file(method, headers, b"index.html", opened.file, opened.meta).await);
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
        if canonical_dir == state.root.canonical_root() && name_bytes == b".pcas" {
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

fn conflict() -> Response {
    (StatusCode::CONFLICT, "Conflict").into_response()
}

fn method_not_allowed(allow_ingest: bool) -> Response {
    let mut response = (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response();
    let allow = if allow_ingest {
        HeaderValue::from_static("GET, HEAD, POST")
    } else {
        HeaderValue::from_static("GET, HEAD")
    };
    response.headers_mut().insert(ALLOW, allow);
    response
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    let body = serde_json::to_vec(&value).expect("serializing a JSON value is infallible");
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string())
            .expect("decimal length is a valid header value"),
    );
    response
}

fn created(indexed: ingest::Ingested) -> Response {
    let digest = indexed.digest.as_str().to_string();
    let mut response = json_response(
        StatusCode::CREATED,
        json!({
            "digest": digest,
            "path": indexed.relative_path,
        }),
    );
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("/pcas/{}", indexed.digest))
            .expect("digest URL is a valid header"),
    );
    response.headers_mut().insert(
        ETAG,
        HeaderValue::from_str(&format!("\"{}\"", indexed.digest))
            .expect("hex digest is a valid entity tag"),
    );
    response
}

fn index_lock_timeout() -> Response {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "error": "index_lock_timeout",
            "published": false,
            "retry": true,
        }),
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

fn cross_device() -> Response {
    json_response(
        StatusCode::CONFLICT,
        json!({
            "error": "cross_filesystem_destination",
            "published": false,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use http_body_util::BodyExt;
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn make_router(root: &Path) -> Router {
        let root = Root::open(root).unwrap();
        router(Arc::new(AppState::new(root)))
    }

    fn make_ingest_router(root: &Path) -> Router {
        let root = Root::open(root).unwrap();
        router(Arc::new(AppState::with_ingestion(
            root,
            IngestionMode::Allow,
        )))
    }

    fn process_fixture() -> &'static Path {
        static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let output = std::env::temp_dir().join(format!(
                    "purecas-process-router-fixture-{}",
                    std::process::id()
                ));
                let source =
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process_fixture.rs");
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
                let status = std::process::Command::new(rustc)
                    .arg(&source)
                    .arg("-O")
                    .arg("-o")
                    .arg(&output)
                    .status()
                    .unwrap();
                assert!(status.success());
                output
            })
            .as_path()
    }

    fn process_routes(
        path: &str,
        args: &[&str],
        max_request_bytes: u64,
        max_concurrency: usize,
    ) -> ProcessRoutes {
        let args = args
            .iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        ProcessRoutes::parse(&format!(
            r#"
[[process_routes]]
path = {path:?}
executable = {executable:?}
args = [{args}]
request_content_type = "application/octet-stream"
response_content_type = "application/octet-stream"
max_request_bytes = {max_request_bytes}
max_concurrency = {max_concurrency}
timeout_seconds = 5
"#,
            executable = process_fixture().to_string_lossy(),
        ))
        .unwrap()
    }

    fn make_process_router(root: &Path, routes: ProcessRoutes, ingestion: IngestionMode) -> Router {
        let root = Root::open(root).unwrap();
        router(Arc::new(AppState::with_options(
            root,
            ingestion,
            Some(routes),
        )))
    }

    async fn request(
        router: Router,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> Response {
        request_with_body(router, method, uri, headers, Body::empty()).await
    }

    async fn request_with_body(
        router: Router,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(body).unwrap();
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

    fn assert_ingest_tmp_empty(root: &Path) {
        let tmp = root.join(".pcas/ingest-tmp");
        if tmp.exists() {
            assert_eq!(
                std::fs::read_dir(tmp).unwrap().count(),
                0,
                "ingestion temporary directory must be empty"
            );
        }
    }

    /// Write one file and index it, returning the resulting digest as a
    /// lowercase hex string.
    fn index_one(root: &Path, name: &str, content: &[u8]) -> String {
        std::fs::write(root.join(name), content).unwrap();
        let report = crate::index::index_root(root, None, false).unwrap();
        report.created[0].digest.as_str().to_string()
    }

    fn boundary_from_content_type(content_type: &str) -> String {
        content_type
            .split("boundary=")
            .nth(1)
            .expect("multipart Content-Type carries a boundary")
            .to_string()
    }

    /// Rebuild the exact expected `multipart/byteranges` body from its
    /// parts, mirroring the wire format the server renders.
    fn expected_multipart_body(
        boundary: &str,
        mime: &str,
        full_len: usize,
        parts: &[(u64, u64, &[u8])],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for (start, end, data) in parts {
            out.extend_from_slice(
                format!("--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: bytes {start}-{end}/{full_len}\r\n\r\n")
                    .as_bytes(),
            );
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
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
        let etag = header(&response, "etag").unwrap().to_string();
        assert_eq!(body_bytes(response).await, b"<h1>hi</h1>");

        let conditional = request(
            make_router(dir.path()),
            Method::GET,
            "/sub/",
            &[("if-none-match", &etag)],
        )
        .await;
        assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
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
    async fn dot_pcas_created_after_router_start_remains_hidden() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"indexed later").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join(".pcas/index.lock"),
            dir.path().join("late-internal"),
        )
        .unwrap();
        let app = make_router(dir.path());

        let report = crate::index::index_root(dir.path(), None, false).unwrap();
        let digest = report.created[0].digest.as_str();

        assert_eq!(
            get(app.clone(), "/.pcas/index.lock").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(app.clone(), "/late-internal").await.status(),
            StatusCode::NOT_FOUND
        );
        let digest_response = get(app, &format!("/pcas/{digest}")).await;
        assert_eq!(digest_response.status(), StatusCode::OK);
        assert_eq!(body_bytes(digest_response).await, b"indexed later");
    }

    #[tokio::test]
    async fn nested_dot_pcas_is_visible_in_directory_listing() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("visible/.pcas")).unwrap();
        std::fs::write(dir.path().join("visible/.pcas/data.txt"), b"visible").unwrap();

        let listing = get(make_router(dir.path()), "/visible/").await;
        assert_eq!(listing.status(), StatusCode::OK);
        let html = String::from_utf8(body_bytes(listing).await).unwrap();
        assert!(html.contains("href=\".pcas/\">.pcas/</a>"), "{html}");

        let file = get(make_router(dir.path()), "/visible/.pcas/data.txt").await;
        assert_eq!(file.status(), StatusCode::OK);
        assert_eq!(body_bytes(file).await, b"visible");
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
    async fn enabled_upload_indexes_exact_nested_path_and_reads_back_ranges() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("unrelated.bin"), b"must remain unindexed").unwrap();
        let content = b"0123456789 uploaded";

        let uploaded = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/nested/a%20b.bin",
            &[],
            Body::from(content.as_slice()),
        )
        .await;

        assert_eq!(uploaded.status(), StatusCode::CREATED);
        let location = header(&uploaded, "location").unwrap().to_string();
        let etag = header(&uploaded, "etag").unwrap().to_string();
        assert!(!etag.starts_with("W/"));
        let payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(uploaded).await).unwrap();
        let digest = payload["digest"].as_str().unwrap();
        assert_eq!(location, format!("/pcas/{digest}"));
        assert_eq!(etag, format!("\"{digest}\""));
        assert_eq!(payload["path"], "nested/a%20b.bin");
        assert_eq!(
            std::fs::read(dir.path().join("nested/a b.bin")).unwrap(),
            content
        );

        let hierarchy = request(
            make_ingest_router(dir.path()),
            Method::GET,
            "/nested/a%20b.bin",
            &[("range", "bytes=2-5")],
        )
        .await;
        assert_eq!(hierarchy.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_bytes(hierarchy).await, b"2345");

        let digest_uri = format!("/pcas/{digest}");
        let by_digest = request(
            make_ingest_router(dir.path()),
            Method::GET,
            &digest_uri,
            &[("range", "bytes=11-18")],
        )
        .await;
        assert_eq!(by_digest.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_bytes(by_digest).await, b"uploaded");

        let unrelated_digest = crate::store::hash_file(&dir.path().join("unrelated.bin")).unwrap();
        assert!(matches!(
            crate::index::resolve_digest(dir.path(), &unrelated_digest),
            Err(crate::index::DigestResolutionError::NotFound(_))
        ));
        assert!(!dir.path().join("purecas.db").exists());
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn upload_streams_many_large_chunks_without_buffering_the_body() {
        let dir = TempDir::new().unwrap();
        let chunk = Bytes::from(vec![0x5a; 16 * 1024]);
        let chunks = (0..256)
            .map(|_| Ok::<_, std::io::Error>(chunk.clone()))
            .collect::<Vec<_>>();
        let body = Body::from_stream(futures_util::stream::iter(chunks));

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/large.bin",
            &[],
            body,
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            std::fs::metadata(dir.path().join("large.bin"))
                .unwrap()
                .len(),
            4 * 1024 * 1024
        );
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn upload_uses_visible_suffix_for_digest_mime() {
        let dir = TempDir::new().unwrap();
        let uploaded = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/movie.mp4",
            &[],
            Body::from("video bytes"),
        )
        .await;
        assert_eq!(uploaded.status(), StatusCode::CREATED);
        let payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(uploaded).await).unwrap();
        let digest = payload["digest"].as_str().unwrap();

        let response = get(make_ingest_router(dir.path()), &format!("/pcas/{digest}")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "content-type"), Some("video/mp4"));
    }

    #[tokio::test]
    async fn upload_collision_is_409_and_preserves_existing_bytes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.bin"), b"original").unwrap();

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/existing.bin",
            &[],
            Body::from("replacement"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read(dir.path().join("existing.bin")).unwrap(),
            b"original"
        );
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn concurrent_same_path_uploads_have_one_create_only_winner() {
        let dir = TempDir::new().unwrap();
        let app = make_ingest_router(dir.path());

        let first = request_with_body(
            app.clone(),
            Method::POST,
            "/race.bin",
            &[],
            Body::from("first"),
        );
        let second = request_with_body(app, Method::POST, "/race.bin", &[], Body::from("second"));
        let (first, second) = tokio::join!(first, second);

        let mut statuses = [first.status(), second.status()];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::CREATED, StatusCode::CONFLICT]);
        let bytes = std::fs::read(dir.path().join("race.bin")).unwrap();
        assert!(bytes == b"first" || bytes == b"second");
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn duplicate_content_upload_succeeds_with_canonical_digest_and_inode() {
        let dir = TempDir::new().unwrap();

        let first = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/first.bin",
            &[],
            Body::from("same content"),
        )
        .await;
        assert_eq!(first.status(), StatusCode::CREATED);
        let first_payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(first).await).unwrap();

        let second = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/second.bin",
            &[],
            Body::from("same content"),
        )
        .await;
        assert_eq!(second.status(), StatusCode::CREATED);
        let second_payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(second).await).unwrap();

        assert_eq!(first_payload["digest"], second_payload["digest"]);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("first.bin"))
                .unwrap()
                .ino(),
            std::fs::metadata(dir.path().join("second.bin"))
                .unwrap()
                .ino()
        );
    }

    #[tokio::test]
    async fn duplicate_upload_repairs_corrupt_canonical_without_losing_uploaded_bytes() {
        let dir = TempDir::new().unwrap();
        let original = b"original content";

        let first = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/first.bin",
            &[],
            Body::from(original.as_slice()),
        )
        .await;
        assert_eq!(first.status(), StatusCode::CREATED);
        let first_payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(first).await).unwrap();
        let digest = first_payload["digest"].as_str().unwrap();

        std::fs::write(dir.path().join("first.bin"), b"corrupt content!").unwrap();

        let second = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/second.bin",
            &[],
            Body::from(original.as_slice()),
        )
        .await;
        assert_eq!(second.status(), StatusCode::CREATED);
        let second_payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(second).await).unwrap();
        assert_eq!(second_payload["digest"], digest);
        assert_eq!(
            std::fs::read(dir.path().join("second.bin")).unwrap(),
            original
        );

        let by_digest = get(make_ingest_router(dir.path()), &format!("/pcas/{digest}")).await;
        assert_eq!(by_digest.status(), StatusCode::OK);
        assert_eq!(body_bytes(by_digest).await, original);
    }

    #[tokio::test]
    async fn upload_rejects_invalid_reserved_and_directory_paths() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        let digest = "a".repeat(64);
        let cases = [
            ("/", StatusCode::NOT_FOUND, None),
            ("/directory/", StatusCode::NOT_FOUND, None),
            ("/directory", StatusCode::NOT_FOUND, None),
            ("/a/./b", StatusCode::NOT_FOUND, None),
            ("/a/%2e%2e/b", StatusCode::NOT_FOUND, None),
            ("/.pcas/object", StatusCode::NOT_FOUND, None),
            ("/purecas.db", StatusCode::NOT_FOUND, None),
            ("/bad%zz", StatusCode::BAD_REQUEST, None),
        ];
        for (uri, expected, allow) in cases {
            let response = request_with_body(
                make_ingest_router(dir.path()),
                Method::POST,
                uri,
                &[],
                Body::from("content"),
            )
            .await;
            assert_eq!(response.status(), expected, "{uri}");
            assert_eq!(header(&response, "allow"), allow, "{uri}");
        }

        let digest_response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            &format!("/pcas/{digest}"),
            &[],
            Body::from("content"),
        )
        .await;
        assert_eq!(digest_response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header(&digest_response, "allow"), Some("GET, HEAD"));

        let unsupported =
            request(make_ingest_router(dir.path()), Method::PUT, "/new.bin", &[]).await;
        assert_eq!(unsupported.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header(&unsupported, "allow"), Some("GET, HEAD, POST"));
    }

    #[tokio::test]
    async fn upload_rejects_symlink_parent_escape() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/escape/file.bin",
            &[],
            Body::from("secret"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!outside.path().join("file.bin").exists());
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn upload_rejects_symlinked_internal_object_directory() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".pcas")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".pcas/sha256")).unwrap();

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/safe.bin",
            &[],
            Body::from("content"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!dir.path().join("safe.bin").exists());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn body_stream_error_removes_temp_and_never_publishes() {
        let dir = TempDir::new().unwrap();
        let chunks = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("disconnected")),
        ];
        let body = Body::from_stream(futures_util::stream::iter(chunks));

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/incomplete.bin",
            &[],
            body,
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!dir.path().join("incomplete.bin").exists());
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn index_lock_timeout_is_retryable_without_visible_publication() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".pcas")).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join(".pcas/index.lock"))
            .unwrap();
        lock.try_lock().unwrap();

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/waiting.bin",
            &[],
            Body::from("not yet published"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header(&response, "retry-after"), Some("1"));
        let payload: serde_json::Value =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(payload["error"], "index_lock_timeout");
        assert_eq!(payload["published"], false);
        assert_eq!(payload["retry"], true);
        assert!(!dir.path().join("waiting.bin").exists());
        assert!(!dir.path().join(".pcas/sha256").exists());
        assert_ingest_tmp_empty(dir.path());
    }

    #[tokio::test]
    async fn index_failure_does_not_publish_visible_file() {
        let dir = TempDir::new().unwrap();
        let corrupt_shard = dir.path().join(".pcas/sha256/aa");
        std::fs::create_dir_all(&corrupt_shard).unwrap();
        std::fs::write(corrupt_shard.join("not-an-object"), b"corrupt").unwrap();

        let response = request_with_body(
            make_ingest_router(dir.path()),
            Method::POST,
            "/failed.bin",
            &[],
            Body::from("must not publish"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!dir.path().join("failed.bin").exists());
        assert_ingest_tmp_empty(dir.path());
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

    // --- configured process routes -------------------------------------

    #[tokio::test]
    async fn process_post_precedes_ingestion_while_get_remains_hierarchy() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("run")).unwrap();
        std::fs::write(dir.path().join("run/7"), b"stored hierarchy bytes").unwrap();
        let routes = process_routes("/run/{stream:u32}", &["argv", "{stream}"], 16, 1);
        let app = make_process_router(dir.path(), routes, IngestionMode::Allow);

        let processed = request_with_body(
            app.clone(),
            Method::POST,
            "/run/7",
            &[("content-type", "application/octet-stream")],
            Body::empty(),
        )
        .await;
        assert_eq!(processed.status(), StatusCode::OK);
        assert_eq!(
            header(&processed, "content-type"),
            Some("application/octet-stream")
        );
        assert_eq!(body_bytes(processed).await, b"7\n");

        let encoded = request_with_body(
            app.clone(),
            Method::POST,
            "/%72un/7",
            &[("content-type", "application/octet-stream")],
            Body::empty(),
        )
        .await;
        assert_eq!(encoded.status(), StatusCode::OK);
        assert_eq!(body_bytes(encoded).await, b"7\n");

        let stored = get(app, "/run/7").await;
        assert_eq!(stored.status(), StatusCode::OK);
        assert_eq!(body_bytes(stored).await, b"stored hierarchy bytes");
    }

    #[tokio::test]
    async fn process_route_enforces_mime_method_and_typed_capture() {
        let dir = TempDir::new().unwrap();
        let routes = process_routes("/run/{stream:u32}", &["argv", "{stream}"], 16, 1);
        let app = make_process_router(dir.path(), routes, IngestionMode::Allow);

        let wrong_mime = request_with_body(
            app.clone(),
            Method::POST,
            "/run/7",
            &[("content-type", "text/plain")],
            Body::empty(),
        )
        .await;
        assert_eq!(wrong_mime.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let missing_mime =
            request_with_body(app.clone(), Method::POST, "/run/7", &[], Body::empty()).await;
        assert_eq!(missing_mime.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let invalid_capture = request_with_body(
            app.clone(),
            Method::POST,
            "/run/01",
            &[("content-type", "application/octet-stream")],
            Body::from("must not become a file"),
        )
        .await;
        assert_eq!(invalid_capture.status(), StatusCode::NOT_FOUND);
        assert!(!dir.path().join("run/01").exists());
        let encoded_invalid = request_with_body(
            app.clone(),
            Method::POST,
            "/run/%30%31",
            &[("content-type", "application/octet-stream")],
            Body::from("must not become a file"),
        )
        .await;
        assert_eq!(encoded_invalid.status(), StatusCode::NOT_FOUND);

        let put = request(app, Method::PUT, "/run/7", &[]).await;
        assert_eq!(put.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header(&put, "allow"), Some("GET, HEAD, POST"));
    }

    #[tokio::test]
    async fn process_route_content_length_and_concurrency_fail_promptly() {
        let dir = TempDir::new().unwrap();
        let routes = process_routes("/run", &["descendant"], 4, 1);
        let app = make_process_router(dir.path(), routes, IngestionMode::ReadOnly);

        let too_large = request_with_body(
            app.clone(),
            Method::POST,
            "/run",
            &[
                ("content-type", "application/octet-stream"),
                ("content-length", "5"),
            ],
            Body::from("12345"),
        )
        .await;
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let active = request_with_body(
            app.clone(),
            Method::POST,
            "/run",
            &[("content-type", "application/octet-stream")],
            Body::empty(),
        )
        .await;
        assert_eq!(active.status(), StatusCode::OK);

        let saturated = request_with_body(
            app,
            Method::POST,
            "/run",
            &[("content-type", "application/octet-stream")],
            Body::empty(),
        )
        .await;
        assert_eq!(saturated.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header(&saturated, "retry-after"), Some("1"));
        drop(active);
    }

    #[tokio::test]
    async fn process_route_rejects_http_1_0_before_spawn() {
        let dir = TempDir::new().unwrap();
        let routes = process_routes("/run", &["argv", "never-spawned"], 4, 1);
        let app = make_process_router(dir.path(), routes, IngestionMode::ReadOnly);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/run")
            .version(http::Version::HTTP_10)
            .header("content-type", "application/octet-stream")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::HTTP_VERSION_NOT_SUPPORTED);
    }

    // --- digest route: identity, headers, normalization, errors --------

    #[tokio::test]
    async fn hierarchy_and_digest_serve_identical_full_bytes() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "data.bin", b"identical bytes everywhere");

        let hierarchy = get(make_router(dir.path()), "/data.bin").await;
        let via_digest = get(make_router(dir.path()), &format!("/pcas/{digest}")).await;
        assert_eq!(hierarchy.status(), StatusCode::OK);
        assert_eq!(via_digest.status(), StatusCode::OK);
        let hierarchy_body = body_bytes(hierarchy).await;
        let digest_body = body_bytes(via_digest).await;
        assert_eq!(hierarchy_body, b"identical bytes everywhere");
        assert_eq!(hierarchy_body, digest_body);
    }

    #[tokio::test]
    async fn digest_route_headers_are_exact() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "movie.mp4", b"video bytes");

        let response = get(make_router(dir.path()), &format!("/pcas/{digest}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, "etag"),
            Some(format!("\"{digest}\"").as_str())
        );
        assert_eq!(
            header(&response, "cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(header(&response, "content-type"), Some("video/mp4"));
        assert_eq!(header(&response, "accept-ranges"), Some("bytes"));
        assert!(header(&response, "last-modified").is_some());
    }

    #[tokio::test]
    async fn digest_route_falls_back_to_octet_stream_without_suffix() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "no-extension", b"opaque bytes");
        let response = get(make_router(dir.path()), &format!("/pcas/{digest}")).await;
        assert_eq!(
            header(&response, "content-type"),
            Some("application/octet-stream")
        );
    }

    #[tokio::test]
    async fn digest_route_normalizes_uppercase_hex() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "data.bin", b"case insensitive");
        let response = get(
            make_router(dir.path()),
            &format!("/pcas/{}", digest.to_uppercase()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"case insensitive");
    }

    #[tokio::test]
    async fn digest_route_unknown_digest_is_404() {
        let dir = TempDir::new().unwrap();
        let unknown = "0".repeat(64);
        let response = get(make_router(dir.path()), &format!("/pcas/{unknown}")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn digest_route_malformed_hex_is_404() {
        let dir = TempDir::new().unwrap();
        let bad = format!("z{}", "a".repeat(63));
        let response = get(make_router(dir.path()), &format!("/pcas/{bad}")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn digest_route_bare_trailing_slash_and_extra_segment_are_404() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "data.bin", b"content");
        for uri in [
            "/pcas".to_string(),
            format!("/pcas/{digest}/"),
            format!("/pcas/{digest}/extra"),
        ] {
            let response = get(make_router(dir.path()), &uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn digest_route_ambiguous_shard_is_500() {
        let dir = TempDir::new().unwrap();
        let digest = "a".repeat(64);
        let shard = dir.path().join(".pcas/sha256").join(&digest[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(format!("{digest}--20260722T130016Z")), b"x").unwrap();
        std::fs::write(shard.join(format!("{digest}--20260722T140000Z")), b"x").unwrap();

        let response = get(make_router(dir.path()), &format!("/pcas/{digest}")).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = String::from_utf8(body_bytes(response).await).unwrap();
        assert!(!body.contains(dir.path().to_str().unwrap()), "{body}");
    }

    #[tokio::test]
    async fn digest_route_malformed_shard_entry_is_500() {
        let dir = TempDir::new().unwrap();
        let digest = "b".repeat(64);
        let shard = dir.path().join(".pcas/sha256").join(&digest[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join("not-a-valid-object-name"), b"x").unwrap();

        let response = get(make_router(dir.path()), &format!("/pcas/{digest}")).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn digest_route_never_creates_sqlite() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "data.bin", b"content");
        assert_eq!(
            get(make_router(dir.path()), &format!("/pcas/{digest}"))
                .await
                .status(),
            StatusCode::OK
        );
        assert!(!dir.path().join("purecas.db").exists());
    }

    // --- byte ranges: forms, clamping, malformed vs. unsatisfiable ------

    #[tokio::test]
    async fn range_closed_form_returns_exact_bytes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=2-5")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 2-5/10"));
        assert_eq!(header(&response, "content-length"), Some("4"));
        assert_eq!(body_bytes(response).await, b"2345");
    }

    #[tokio::test]
    async fn range_open_ended_form_runs_to_end() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=8-")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 8-9/10"));
        assert_eq!(body_bytes(response).await, b"89");
    }

    #[tokio::test]
    async fn range_suffix_form_returns_last_bytes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=-3")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 7-9/10"));
        assert_eq!(body_bytes(response).await, b"789");
    }

    #[tokio::test]
    async fn range_oversized_suffix_is_the_entire_representation() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=-1000")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 0-9/10"));
        assert_eq!(body_bytes(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn range_closed_end_is_clamped_to_length() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=5-9999")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 5-9/10"));
        assert_eq!(body_bytes(response).await, b"56789");
    }

    #[tokio::test]
    async fn range_mixed_satisfiable_and_unsatisfiable_keeps_satisfiable() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=1000-2000,0-3")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 0-3/10"));
        assert_eq!(body_bytes(response).await, b"0123");
    }

    #[tokio::test]
    async fn range_reversed_is_400() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=9-2")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn range_malformed_grammar_is_400() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=a-b")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn range_overflowing_integer_is_400() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=99999999999999999999-")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn range_unsupported_unit_is_ignored_and_returns_full_200() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "items=0-2")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn range_against_empty_file_is_416() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("empty.bin"), b"").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/empty.bin",
            &[("range", "bytes=0-0")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header(&response, "content-range"), Some("bytes */0"));
    }

    #[tokio::test]
    async fn range_wholly_unsatisfiable_is_416_with_content_range() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=1000-2000")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header(&response, "content-range"), Some("bytes */10"));
        assert!(header(&response, "etag").is_some());
    }

    #[tokio::test]
    async fn range_excessive_raw_specs_is_416() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), vec![b'x'; 1000]).unwrap();
        let spec = (0..65)
            .map(|i| format!("{}-{}", i * 2, i * 2))
            .collect::<Vec<_>>()
            .join(",");
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", &format!("bytes={spec}"))],
        )
        .await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn range_excessive_coalesced_ranges_is_416() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), vec![b'x'; 1000]).unwrap();
        // 17 disjoint single-byte ranges: none adjacent, so none coalesce.
        let spec = (0..17)
            .map(|i| format!("{}-{}", i * 2, i * 2))
            .collect::<Vec<_>>()
            .join(",");
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", &format!("bytes={spec}"))],
        )
        .await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn duplicate_range_header_lines_combine_in_field_order() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let mut req = Request::builder()
            .method(Method::GET)
            .uri("/f.bin")
            .body(Body::empty())
            .unwrap();
        req.headers_mut()
            .append("range", HeaderValue::from_static("bytes=0-1"));
        req.headers_mut()
            .append("range", HeaderValue::from_static("bytes=8-9"));
        let response = make_router(dir.path()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(header(&response, "content-type")
            .unwrap()
            .starts_with("multipart/byteranges"));
        let content_type = header(&response, "content-type").unwrap().to_string();
        let boundary = boundary_from_content_type(&content_type);
        let expected = expected_multipart_body(
            &boundary,
            "application/octet-stream",
            10,
            &[(0, 1, b"01"), (8, 9, b"89")],
        );
        assert_eq!(body_bytes(response).await, expected);
    }

    #[tokio::test]
    async fn overlapping_and_adjacent_ranges_are_coalesced() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=0-3,2-5,6-7")],
        )
        .await;
        // 0-3 and 2-5 overlap; 6-7 is adjacent to the merged 0-5: the
        // whole set coalesces into one 0-9 range.
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header(&response, "content-range"), Some("bytes 0-7/10"));
        assert_eq!(body_bytes(response).await, b"01234567");
    }

    // --- multipart: exact bytes, exact Content-Length, sequential I/O ---

    #[tokio::test]
    async fn multipart_body_and_content_length_are_exact_across_digit_widths() {
        let dir = TempDir::new().unwrap();
        let content: Vec<u8> = (0..1000).map(|i| b'a' + (i % 26) as u8).collect();
        std::fs::write(dir.path().join("data.txt"), &content).unwrap();

        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/data.txt",
            &[("range", "bytes=0-4,50-59,900-999")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let content_type = header(&response, "content-type").unwrap().to_string();
        assert!(content_type.starts_with("multipart/byteranges; boundary="));
        let boundary = boundary_from_content_type(&content_type);
        let declared_length: usize = header(&response, "content-length")
            .unwrap()
            .parse()
            .unwrap();

        let body = body_bytes(response).await;
        let expected = expected_multipart_body(
            &boundary,
            "text/plain",
            1000,
            &[
                (0, 4, &content[0..5]),
                (50, 59, &content[50..60]),
                (900, 999, &content[900..1000]),
            ],
        );
        assert_eq!(declared_length, body.len());
        assert_eq!(body, expected);
    }

    // --- If-Range: strong match, mismatch, weak-never, date, malformed --

    #[tokio::test]
    async fn if_range_strong_digest_etag_match_honors_range() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "data.bin", b"0123456789");
        let uri = format!("/pcas/{digest}");
        let response = request(
            make_router(dir.path()),
            Method::GET,
            &uri,
            &[
                ("range", "bytes=0-3"),
                ("if-range", &format!("\"{digest}\"")),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_bytes(response).await, b"0123");
    }

    #[tokio::test]
    async fn if_range_strong_digest_etag_mismatch_ignores_range() {
        let dir = TempDir::new().unwrap();
        let digest = index_one(dir.path(), "data.bin", b"0123456789");
        let uri = format!("/pcas/{digest}");
        let other = "f".repeat(64);
        let response = request(
            make_router(dir.path()),
            Method::GET,
            &uri,
            &[
                ("range", "bytes=0-3"),
                ("if-range", &format!("\"{other}\"")),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn if_range_weak_hierarchy_etag_never_satisfies_tag_form() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let current_etag = header(&get(make_router(dir.path()), "/f.bin").await, "etag")
            .unwrap()
            .to_string();
        assert!(current_etag.starts_with("W/\""));

        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=0-3"), ("if-range", &current_etag)],
        )
        .await;
        // A weak validator can never satisfy If-Range, even when it is
        // (textually) the resource's own current ETag: the range is
        // ignored and the full representation is served.
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn if_range_date_not_modified_since_honors_range() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let last_modified = header(
            &get(make_router(dir.path()), "/f.bin").await,
            "last-modified",
        )
        .unwrap()
        .to_string();

        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=0-3"), ("if-range", &last_modified)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_bytes(response).await, b"0123");
    }

    #[tokio::test]
    async fn if_range_date_modified_since_ignores_range() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();

        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[
                ("range", "bytes=0-3"),
                ("if-range", "Mon, 01 Jan 1990 00:00:00 GMT"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_bytes(response).await, b"0123456789");
    }

    #[tokio::test]
    async fn if_range_malformed_is_400() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=0-3"), ("if-range", "not-a-valid-if-range")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn normal_preconditions_win_over_range_processing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let etag = header(&get(make_router(dir.path()), "/f.bin").await, "etag")
            .unwrap()
            .to_string();

        let response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("if-none-match", &etag), ("range", "bytes=0-3")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(body_bytes(response).await.is_empty());
    }

    // --- HEAD mirrors full/single/multipart GET status and headers -----

    #[tokio::test]
    async fn head_mirrors_full_response_headers_with_empty_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let get_response = get(make_router(dir.path()), "/f.bin").await;
        let head_response = request(make_router(dir.path()), Method::HEAD, "/f.bin", &[]).await;
        assert_eq!(head_response.status(), get_response.status());
        for name in ["content-length", "content-type", "etag", "accept-ranges"] {
            assert_eq!(header(&get_response, name), header(&head_response, name));
        }
        assert!(body_bytes(head_response).await.is_empty());
    }

    #[tokio::test]
    async fn head_mirrors_single_range_response_headers_with_empty_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let get_response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=2-5")],
        )
        .await;
        let head_response = request(
            make_router(dir.path()),
            Method::HEAD,
            "/f.bin",
            &[("range", "bytes=2-5")],
        )
        .await;
        assert_eq!(head_response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(head_response.status(), get_response.status());
        for name in ["content-length", "content-range", "content-type"] {
            assert_eq!(header(&get_response, name), header(&head_response, name));
        }
        assert!(body_bytes(head_response).await.is_empty());
    }

    #[tokio::test]
    async fn head_mirrors_multipart_response_headers_with_empty_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let get_response = request(
            make_router(dir.path()),
            Method::GET,
            "/f.bin",
            &[("range", "bytes=0-1,8-9")],
        )
        .await;
        let head_response = request(
            make_router(dir.path()),
            Method::HEAD,
            "/f.bin",
            &[("range", "bytes=0-1,8-9")],
        )
        .await;
        assert_eq!(head_response.status(), StatusCode::PARTIAL_CONTENT);
        for name in ["content-length", "content-type"] {
            assert_eq!(header(&get_response, name), header(&head_response, name));
        }
        assert!(body_bytes(head_response).await.is_empty());
    }

    #[tokio::test]
    async fn head_mirrors_416_response_headers_with_empty_body() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.bin"), b"0123456789").unwrap();
        let head_response = request(
            make_router(dir.path()),
            Method::HEAD,
            "/f.bin",
            &[("range", "bytes=1000-2000")],
        )
        .await;
        assert_eq!(head_response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header(&head_response, "content-range"), Some("bytes */10"));
        assert!(body_bytes(head_response).await.is_empty());
    }
}
