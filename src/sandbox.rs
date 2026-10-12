//! The sandbox cluster: three fake backends plus a router wired to them, so
//! load balancing, sticky sessions and the dashboard can be watched without
//! any real backends.
//!
//! The router listens on `127.0.0.1:<port>` (`PORT` env var, default 6060); the
//! fake backends take the next three ports.

use crate::app::run_until_shutdown;
use crate::sandbox_server::{start_sandbox_server, DEFAULT_MODELS};
use crate::types::{BackendConfig, Config};

/// The three fake-backend ports that follow the router's.
fn backend_ports(port: u16) -> [u16; 3] {
    [
        port.saturating_add(1),
        port.saturating_add(2),
        port.saturating_add(3),
    ]
}

fn sandbox_backends(port: u16) -> Vec<BackendConfig> {
    backend_ports(port)
        .iter()
        .enumerate()
        .map(|(i, backend_port)| BackendConfig {
            id: format!("sandbox-{}", i + 1),
            base_url: format!("http://127.0.0.1:{backend_port}/v1"),
            weight: 1.0,
            model: None,
            api_key: None,
            priority: None,
        })
        .collect()
}

/// Router port for the sandbox, from the `PORT` env var (default 6060).
pub fn sandbox_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|port| port.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
        .unwrap_or(6060)
}

/// The sandbox router configuration.
pub fn sandbox_config(host: &str, port: u16) -> Config {
    Config {
        host: host.to_string(),
        port,
        backends: sandbox_backends(port),
        sticky: true,
        health_check_interval_ms: 5_000,
        health_check_timeout_ms: 2_000,
        session_ttl_ms: 30 * 60_000,
        upstream_timeout_ms: 120_000,
        max_retries: 2,
        models: Some(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect()),
        ..Config::default()
    }
}

/// Start the fake backends and run the sandbox router until shutdown.
pub async fn run_sandbox(host: &str, port: u16) -> ! {
    let config = sandbox_config(host, port);
    let backend_ports = backend_ports(config.port);

    // Keep the backend handles alive for the lifetime of the process.
    let mut backends: Vec<crate::app::Running> = Vec::new();
    for port in &backend_ports {
        backends.push(
            start_sandbox_server(
                *port,
                DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(),
                None,
            )
            .await,
        );
    }
    std::mem::forget(backends);

    println!(
        "[aiproxy-sandbox] fake backends on {}",
        backend_ports
            .iter()
            .map(|port| format!("127.0.0.1:{port}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    run_until_shutdown(config).await;
}
