//! A tiny helper for tests that need real HTTP servers on ephemeral ports
//! (the Deno tests used `Deno.serve({ port: 0 })` for the same purpose).

use axum::Router;
use std::net::SocketAddr;

/// An ephemeral-port HTTP server that shuts down when dropped.
pub struct TestServer {
    pub port: u16,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TestServer {
    /// Base URL of the server, e.g. `http://127.0.0.1:34567`.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Full URL for a path, e.g. `/v1/models`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    pub async fn close(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Start `app` on 127.0.0.1:0 and return the handle with its assigned port.
pub async fn spawn_test_server(app: Router) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        server.await.ok();
    });
    TestServer {
        port,
        shutdown: Some(shutdown_tx),
        task: Some(task),
    }
}

/// Listen on an ephemeral port and start serving `app`, returning the bound
/// address (useful for the router itself, not just test doubles).
pub async fn spawn_router(
    app: Router,
) -> (
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        server.await.ok();
    });
    (addr, shutdown_tx, task)
}
