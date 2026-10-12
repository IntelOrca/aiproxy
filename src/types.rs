//! Shared types for the router.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single backend as configured in `config.json`.
#[derive(Clone, Debug)]
pub struct BackendConfig {
    /// Unique identifier, shown in logs and the dashboard.
    pub id: String,
    /// OpenAI-compatible API root, e.g. `http://127.0.0.1:3456/v1`.
    pub base_url: String,
    /// Relative share of new conversations.
    pub weight: f64,
    /// Optional model name substituted when forwarding to this backend.
    pub model: Option<String>,
    /// API key sent to this backend (overrides the client's Authorization).
    pub api_key: Option<String>,
    /// Routing preference: lower = tried first while healthy. Backends without
    /// a priority rank equally and are only used when no prioritized backend is
    /// healthy. Set 0 on the primary, 1 on the fallbacks.
    pub priority: Option<i64>,
}

impl BackendConfig {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            weight: 1.0,
            model: None,
            api_key: None,
            priority: None,
        }
    }

    /// Backends without an explicit priority rank below any prioritized one.
    pub fn effective_priority(&self) -> i64 {
        self.priority.unwrap_or(i64::MAX)
    }
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self::new("", "")
    }
}

/// Router configuration. Mirrors the Deno version's `Config`, including which
/// fields fall back to defaults and how.
#[derive(Clone, Debug)]
pub struct Config {
    /// Host to listen on. `127.0.0.1` unless the config or `-b` says otherwise;
    /// `0.0.0.0` means every interface.
    pub host: String,
    pub port: u16,
    pub backends: Vec<BackendConfig>,
    /// Route each conversation to the same backend (recommended for cache affinity).
    pub sticky: bool,
    pub health_check_interval_ms: u64,
    pub health_check_timeout_ms: u64,
    pub session_ttl_ms: u64,
    pub upstream_timeout_ms: u64,
    pub max_retries: u64,
    /// Static model list served by GET /v1/models. If unset, proxied from a healthy backend.
    pub models: Option<Vec<String>>,
    /// Client model name -> backend model name, applied on forward.
    pub model_map: Option<HashMap<String, String>>,
    /// Client API keys required on incoming requests (router auth).
    pub api_keys: Option<Vec<String>>,
    /// Upstream status codes treated as rate limited (default [429, 529]).
    pub retry_status_codes: Option<Vec<u16>>,
    /// Also fail over when an upstream error body mentions rate limiting / quota.
    pub retry_on_limit_message: bool,
    /// Directory for persisted state (pins, stats).
    pub state_dir: Option<String>,
    /// How many recent route entries to keep and persist.
    pub recent_routes: u64,
    /// How long to wait for in-flight requests to drain on shutdown.
    pub shutdown_grace_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            // Note: 0 is not a meaningful config value (JS `0 || default`),
            // but it is used directly by tests to request an ephemeral port.
            port: 8080,
            backends: Vec::new(),
            sticky: true,
            health_check_interval_ms: 10_000,
            health_check_timeout_ms: 2_500,
            session_ttl_ms: 30 * 60_000,
            upstream_timeout_ms: 600_000,
            max_retries: 1,
            models: None,
            model_map: None,
            api_keys: None,
            retry_status_codes: None,
            retry_on_limit_message: true,
            state_dir: None,
            recent_routes: 200,
            shutdown_grace_ms: 10_000,
        }
    }
}

impl Config {
    /// Effective retry status codes, falling back to the built-in default.
    pub fn retry_codes(&self) -> Vec<u16> {
        self.retry_status_codes
            .clone()
            .unwrap_or_else(|| DEFAULT_RETRY_STATUS_CODES.to_vec())
    }

    /// True when client authentication is enabled.
    pub fn auth_required(&self) -> bool {
        self.api_keys.as_ref().is_some_and(|k| !k.is_empty())
    }
}

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_RETRY_STATUS_CODES: [u16; 2] = [429, 529];

/// A persisted session -> backend pin.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredPin {
    #[serde(rename = "backendId")]
    pub backend_id: String,
    #[serde(rename = "lastSeen")]
    pub last_seen: u64,
}

/// A single routed request, shown in the dashboard and persisted for restarts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RoutingEntry {
    pub at: String,
    pub session_id: String,
    pub model: String,
    pub backend_id: String,
    pub endpoint: String,
    pub status: u16,
    pub ms: u64,
}
