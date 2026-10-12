//! aiproxy — a local, OpenAI-compatible completions router and load balancer.

use aiproxy::app::run_until_shutdown;
use aiproxy::bind::{resolve_listen, resolve_listen_from_config};
use aiproxy::config::{load_config_at, resolve_config_path};
use aiproxy::log::info;
use aiproxy::sandbox::{run_sandbox, sandbox_port};
use aiproxy::sandbox_server::run_standalone;
use aiproxy::types::DEFAULT_HOST;
use clap::{CommandFactory, Parser};
use std::path::PathBuf;

/// Local, OpenAI-compatible completions router and load balancer.
#[derive(Parser, Debug)]
#[command(name = "aiproxy", version, about, long_about = None)]
struct Args {
    /// Path to the router config file. When omitted, the user config
    /// (~/.config/aiproxy/aiproxy.json) and then the system-wide config
    /// (/etc/aiproxy/aiproxy.json) are tried.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the listen port from the config.
    #[arg(short, long, value_name = "PORT")]
    port: Option<u16>,

    /// Listen address: HOST, HOST:PORT, or *:PORT for every interface.
    #[arg(short, long, value_name = "BIND")]
    bind: Option<String>,

    /// Run the sandbox cluster: a router plus three fake backends.
    #[arg(long)]
    sandbox: bool,

    /// Run a single fake backend (PORT env var, default 6060).
    #[arg(long)]
    sandbox_server: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if args.sandbox_server {
        run_standalone().await;
        return;
    }

    if args.sandbox {
        // The sandbox runs its own router to completion.
        let (host, port) = listen_or_exit(resolve_listen(
            DEFAULT_HOST,
            sandbox_port(),
            args.bind.as_deref(),
            args.port,
        ));
        run_sandbox(&host, port).await;
    }

    let config_path = resolve_config_path(args.config.as_deref());
    let mut config = load_config_at(config_path.as_deref());
    let (host, port) = listen_or_exit(resolve_listen_from_config(
        &config.host,
        config.port,
        config.bind.as_deref(),
        args.bind.as_deref(),
        args.port,
    ));
    config.host = host;
    config.port = port;

    info(format!(
        "config: {} — listening on http://{}:{} — backends: {}",
        config_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "none, using defaults".to_string()),
        config.host,
        config.port,
        config
            .backends
            .iter()
            .map(|backend| format!("{}({})", backend.id, backend.base_url))
            .collect::<Vec<_>>()
            .join(", "),
    ));
    run_until_shutdown(config).await;
}

/// Unwrap a resolved listen address, exiting with a usage error when the
/// `--bind`/`--port` combination is invalid or contradictory.
fn listen_or_exit(address: Result<(String, u16), String>) -> (String, u16) {
    match address {
        Ok(address) => address,
        Err(message) => Args::command()
            .error(clap::error::ErrorKind::ArgumentConflict, message)
            .exit(),
    }
}
