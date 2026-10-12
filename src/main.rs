//! aiproxy — a local, OpenAI-compatible completions router and load balancer.

use aiproxy::app::run_until_shutdown;
use aiproxy::bind::resolve_listen;
use aiproxy::config::load_config;
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
    /// Path to the router config file.
    #[arg(long, default_value = "config.json", value_name = "PATH")]
    config: PathBuf,

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
        let (host, port) = listen_address(DEFAULT_HOST, sandbox_port(), &args);
        run_sandbox(&host, port).await;
    }

    let mut config = load_config(&args.config);
    let (host, port) = listen_address(&config.host, config.port, &args);
    config.host = host;
    config.port = port;

    info(format!(
        "config: {} — listening on http://{}:{} — backends: {}",
        args.config.display(),
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

/// Resolve the effective listen address, exiting with a usage error when `-b`
/// and `-p` contradict each other.
fn listen_address(host: &str, port: u16, args: &Args) -> (String, u16) {
    match resolve_listen(host, port, args.bind.as_deref(), args.port) {
        Ok(address) => address,
        Err(message) => Args::command()
            .error(clap::error::ErrorKind::ArgumentConflict, message)
            .exit(),
    }
}
