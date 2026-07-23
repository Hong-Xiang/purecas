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
pub mod representation;
pub mod resolve;
pub mod router;

mod digest;
mod range;
mod respond;

use anyhow::{Context, Result};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

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
    let root = resolve::Root::open(root)?;
    let state = Arc::new(router::AppState::new(root));
    let app = router::router(state);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let local_addr = listener
        .local_addr()
        .context("reading bound local address")?;
    Ok((local_addr, serve(app, listener, shutdown)))
}

/// The synchronous CLI entry point for `pcas serve`. Builds a dedicated
/// Tokio runtime and blocks on it: this is the only place `pcas` enters an
/// async runtime.
pub fn run_cli(root: &Path, bind: SocketAddr) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .build()
        .context("building the Tokio runtime")?;
    runtime.block_on(async move {
        let (local_addr, server) = bind_and_serve(root, bind, std::future::pending()).await?;
        eprintln!("pcas serve: listening on http://{local_addr}");
        server.await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

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
}
