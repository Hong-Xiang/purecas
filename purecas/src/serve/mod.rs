//! `pcas serve`: a read-only HTTP server for the visible filesystem
//! hierarchy under `PCAS_ROOT`.
//!
//! ## Stack
//!
//! `axum` on Tokio's Hyper-based HTTP/1 server, plus a small set of
//! maintained, narrowly-scoped helpers: `headers` for typed conditional-
//! request header parsing (correct strong/weak `ETag` comparison and
//! `HTTP-date` handling), `mime_guess` for `Content-Type`, and
//! `askama_escape`/`percent_encoding` for safe HTML/URL rendering of
//! directory listings. All are actively maintained, already part of the
//! Tokio/Tower ecosystem (or, for `mime_guess`/`askama_escape`/
//! `percent_encoding`, small single-purpose crates with no heavy
//! transitive footprint), and each is used only where it models the
//! relevant RFC semantics directly — custom policy (path decoding,
//! containment, precondition ordering) stays in this module, small, typed,
//! and directly tested.
//!
//! `tower-http`'s path-only `ServeFile`/`ServeDir` are deliberately not
//! used: they reopen a path after metadata has been derived, which would
//! let a concurrent atomic replacement make the `ETag` describe different
//! bytes than the streamed body. Every visible target here is opened
//! exactly once; representation metadata and body bytes both come from
//! that same descriptor.
//!
//! The async Tokio runtime is entered only for this command; every other
//! `pcas` command remains fully synchronous.

pub mod listing;
pub mod path;
pub mod precondition;
pub mod process;
pub mod representation;
pub mod resolve;
pub mod router;

mod digest;
mod ingest;
mod range;
mod respond;

use anyhow::{Context, Result};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestionMode {
    ReadOnly,
    Allow,
}

#[derive(Clone, Debug)]
pub struct ServerOptions {
    ingestion: IngestionMode,
    process_routes: Option<process::ProcessRoutes>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            ingestion: IngestionMode::ReadOnly,
            process_routes: None,
        }
    }
}

impl ServerOptions {
    pub fn with_ingestion(mut self, ingestion: IngestionMode) -> Self {
        self.ingestion = ingestion;
        self
    }

    pub fn with_process_routes(mut self, routes: process::ProcessRoutes) -> Self {
        self.process_routes = Some(routes);
        self
    }
}

/// Serve `router` on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    router: axum::Router,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serving HTTP")
}

/// Build the router state and bind `bind`, then serve until `shutdown`
/// resolves. Returns the bound local address once the listener is ready,
/// alongside the server future, so callers (including tests) can observe
/// the actual ephemeral port.
pub async fn bind_and_serve(
    root: &Path,
    bind: SocketAddr,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(SocketAddr, impl Future<Output = Result<()>>)> {
    bind_and_serve_with_ingestion(root, bind, IngestionMode::ReadOnly, shutdown).await
}

pub async fn bind_and_serve_with_ingestion(
    root: &Path,
    bind: SocketAddr,
    ingestion: IngestionMode,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(SocketAddr, impl Future<Output = Result<()>>)> {
    bind_and_serve_with_options(
        root,
        bind,
        ServerOptions::default().with_ingestion(ingestion),
        shutdown,
    )
    .await
}

pub async fn bind_and_serve_with_options(
    root: &Path,
    bind: SocketAddr,
    options: ServerOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(SocketAddr, impl Future<Output = Result<()>>)> {
    let root = resolve::Root::open(root)?;
    if options.ingestion == IngestionMode::Allow {
        ingest::cleanup_stale(&root)?;
    }
    let process_routes = options.process_routes.clone();
    let state = Arc::new(router::AppState::with_options(
        root,
        options.ingestion,
        options.process_routes,
    ));
    let app = router::router(state);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let local_addr = listener
        .local_addr()
        .context("reading bound local address")?;
    let shutdown = async move {
        shutdown.await;
        if let Some(routes) = process_routes {
            routes.cancel();
        }
    };
    Ok((local_addr, serve(app, listener, shutdown)))
}

/// The synchronous CLI entry point for `pcas serve`. Builds a dedicated
/// Tokio runtime and blocks on it: this is the only place `pcas` enters an
/// async runtime.
pub fn run_cli(root: &Path, bind: SocketAddr) -> Result<()> {
    run_cli_with_ingestion(root, bind, IngestionMode::ReadOnly)
}

pub fn run_cli_with_ingestion(
    root: &Path,
    bind: SocketAddr,
    ingestion: IngestionMode,
) -> Result<()> {
    run_cli_with_options(
        root,
        bind,
        ServerOptions::default().with_ingestion(ingestion),
    )
}

pub fn run_cli_with_options(root: &Path, bind: SocketAddr, options: ServerOptions) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the Tokio runtime")?;
    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let signal_routes = options.process_routes.clone();
        let mut signal_task = tokio::spawn(cli_signal_sequence(shutdown_tx, signal_routes));
        let (local_addr, server) = bind_and_serve_with_options(root, bind, options, async {
            let _ = shutdown_rx.await;
        })
        .await?;
        eprintln!("pcas serve: listening on http://{local_addr}");
        tokio::pin!(server);
        tokio::select! {
            biased;
            signal = &mut signal_task => {
                signal.context("CLI signal task failed")?;
                std::process::exit(130);
            }
            result = &mut server => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    &mut signal_task,
                )
                .await
                {
                    Ok(signal) => {
                        signal.context("CLI signal task failed")?;
                        std::process::exit(130);
                    }
                    Err(_) => {
                        signal_task.abort();
                        result
                    }
                }
            }
        }
    })
}

async fn cli_signal_sequence(
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    process_routes: Option<process::ProcessRoutes>,
) {
    #[cfg(unix)]
    {
        let mut signals = shutdown_signals();
        receive_shutdown_signal(&mut signals).await;
        if let Some(routes) = &process_routes {
            routes.cancel();
        }
        let _ = shutdown_tx.send(());
        // Keep the same registered streams: a second signal already queued
        // on the non-winning stream remains observable here.
        receive_shutdown_signal(&mut signals).await;
        if let Some(routes) = process_routes {
            routes.terminate_active_groups().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        if let Some(routes) = &process_routes {
            routes.cancel();
        }
        let _ = shutdown_tx.send(());
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
type ShutdownSignals = (
    Option<tokio::signal::unix::Signal>,
    Option<tokio::signal::unix::Signal>,
);

#[cfg(unix)]
fn shutdown_signals() -> ShutdownSignals {
    (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok(),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok(),
    )
}

#[cfg(unix)]
async fn receive_shutdown_signal((interrupt, terminate): &mut ShutdownSignals) {
    tokio::select! {
        _ = async {
            if let Some(signal) = interrupt {
                signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {}
        _ = async {
            if let Some(signal) = terminate {
                signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    fn process_fixture() -> &'static Path {
        static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let output = std::env::temp_dir().join(format!(
                    "purecas-process-socket-fixture-{}",
                    std::process::id()
                ));
                let source =
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process_fixture.rs");
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
                let status = std::process::Command::new(rustc)
                    .arg(source)
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

    fn process_routes(mode: &str) -> process::ProcessRoutes {
        process::ProcessRoutes::parse(&format!(
            r#"
[[process_routes]]
path = "/run"
executable = {executable:?}
args = [{mode:?}]
request_content_type = "application/octet-stream"
response_content_type = "application/octet-stream"
max_request_bytes = 1024
max_concurrency = 1
timeout_seconds = 5
"#,
            executable = process_fixture().to_string_lossy(),
        ))
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_a_nested_file_over_a_real_socket_and_shuts_down_cleanly() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), b"hello from disk").unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (local_addr, server) =
            bind_and_serve(dir.path(), "127.0.0.1:0".parse().unwrap(), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();

        let server_task = tokio::spawn(server);

        let response = reqwest::get(format!("http://{local_addr}/a/b/c.txt"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.bytes().await.unwrap();
        assert_eq!(&body[..], b"hello from disk");

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server shut down within timeout")
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_ranges_and_digest_over_a_real_socket() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();
        let report = crate::index::index_root(dir.path(), None, false).unwrap();
        let digest = report.created[0].digest.as_str().to_string();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (local_addr, server) =
            bind_and_serve(dir.path(), "127.0.0.1:0".parse().unwrap(), async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();

        let server_task = tokio::spawn(server);

        let client = reqwest::Client::new();

        let ranged = client
            .get(format!("http://{local_addr}/data.bin"))
            .header("range", "bytes=2-5")
            .send()
            .await
            .unwrap();
        assert_eq!(ranged.status(), 206);
        assert_eq!(
            ranged.headers().get("content-range").unwrap(),
            "bytes 2-5/10"
        );
        assert_eq!(&ranged.bytes().await.unwrap()[..], b"2345");

        let via_digest = client
            .get(format!("http://{local_addr}/pcas/{digest}"))
            .send()
            .await
            .unwrap();
        assert_eq!(via_digest.status(), 200);
        assert_eq!(
            via_digest.headers().get("etag").unwrap(),
            format!("\"{digest}\"").as_str()
        );
        assert_eq!(&via_digest.bytes().await.unwrap()[..], b"0123456789");

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server shut down within timeout")
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn late_process_failure_aborts_real_http_body() {
        let dir = TempDir::new().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let options = ServerOptions::default().with_process_routes(process_routes("late-fail"));
        let (local_addr, server) = bind_and_serve_with_options(
            dir.path(),
            "127.0.0.1:0".parse().unwrap(),
            options,
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .unwrap();
        let server_task = tokio::spawn(server);

        let result = reqwest::Client::new()
            .post(format!("http://{local_addr}/run"))
            .header("content-type", "application/octet-stream")
            .body("")
            .send()
            .await;
        match result {
            Ok(response) => {
                assert_eq!(response.status(), 200);
                assert!(
                    response.bytes().await.is_err(),
                    "late nonzero exit must truncate/error the transport"
                );
            }
            Err(error) => {
                assert!(
                    error.is_request() || error.is_body(),
                    "unexpected transport error: {error}"
                );
            }
        }

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server shut down within timeout")
            .unwrap()
            .unwrap();
    }
}
