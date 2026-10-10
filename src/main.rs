//! aiproxy — a local, OpenAI-compatible completions router and load balancer.

use aiproxy::app::run_until_shutdown;
use aiproxy::config::load_config;
use aiproxy::log::info;
use aiproxy::sandbox::run_sandbox;
use aiproxy::sandbox_server::run_standalone;
use clap::Parser;
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
        run_sandbox().await;
    }

    let mut config = load_config(&args.config);
    if let Some(port) = args.port {
        config.port = port;
    }

    info(format!(
        "config: {} — listening on http://127.0.0.1:{} — backends: {}",
        args.config.display(),
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
