//! aiproxy — a local, OpenAI-compatible completions router and load balancer.

pub mod app;
pub mod backend;
pub mod bind;
pub mod config;
pub mod dashboard;
pub mod isotime;
pub mod log;
pub mod pins_store;
pub mod sandbox;
pub mod sandbox_server;
pub mod session;
pub mod stats_store;
pub mod testing;
pub mod types;
pub mod upstream;

pub use config::load_config;
pub use types::{BackendConfig, Config, RoutingEntry, StoredPin};
