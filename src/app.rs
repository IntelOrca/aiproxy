//! The router HTTP server: request dispatch, upstream forwarding, dashboard,
//! stats persistence and graceful shutdown.

use crate::backend::{with_path, BackendManager};
use crate::config::js_to_string;
use crate::dashboard::DASHBOARD_HTML;
use crate::isotime::{now_ms, to_iso8601};
use crate::log::{error, info};
use crate::pins_store::default_state_dir;
use crate::session::session_key_from_body;
use crate::stats_store::{load_stats, save_stats, StatsSnapshot};
use crate::types::{Config, RoutingEntry};
use crate::upstream::{forward_with_retry, ForwardOutcome, ForwardRequest};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http::header::AUTHORIZATION;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the router waits between stats flushes.
const STATS_PERSIST_INTERVAL_MS: u64 = 500;
/// Upstream timeout for a proxied `GET /models`.
const MODELS_PROXY_TIMEOUT_MS: u64 = 3_000;

/// Live dashboard stats, persisted to disk on a debounce.
pub struct StatsStore {
    inner: Mutex<StatsInner>,
    state_dir: std::path::PathBuf,
    dirty: AtomicBool,
}

#[derive(Default)]
struct StatsInner {
    backend_counts: BTreeMap<String, u64>,
    recent: VecDeque<RoutingEntry>,
}

impl StatsStore {
    /// Restore stats from the previous run, dropping entries for backends that
    /// are no longer configured.
    pub fn load(config: &Config) -> Self {
        let state_dir = config
            .state_dir
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(default_state_dir);
        let max_recent = max_recent(config);
        let StatsSnapshot {
            backend_counts,
            recent,
        } = load_stats(&state_dir);

        let known_ids: std::collections::HashSet<&str> =
            config.backends.iter().map(|b| b.id.as_str()).collect();
        let recent: VecDeque<RoutingEntry> = recent
            .into_iter()
            .filter(|entry| known_ids.contains(entry.backend_id.as_str()))
            .take(max_recent)
            .collect();
        let backend_counts: BTreeMap<String, u64> = config
            .backends
            .iter()
            .map(|b| {
                (
                    b.id.clone(),
                    backend_counts.get(&b.id).copied().unwrap_or(0),
                )
            })
            .collect();

        Self {
            inner: Mutex::new(StatsInner {
                backend_counts,
                recent,
            }),
            state_dir,
            dirty: AtomicBool::new(false),
        }
    }

    /// Record a routed request: bump the backend's counter and prepend a recent
    /// route entry, capped at `max_recent`.
    pub fn record(&self, entry: RoutingEntry, max_recent: usize) {
        let mut inner = self.lock();
        *inner
            .backend_counts
            .entry(entry.backend_id.clone())
            .or_insert(0) += 1;
        inner.recent.push_front(entry);
        while inner.recent.len() > max_recent {
            inner.recent.pop_back();
        }
        drop(inner);
        self.dirty.store(true, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> (BTreeMap<String, u64>, Vec<RoutingEntry>) {
        let inner = self.lock();
        (
            inner.backend_counts.clone(),
            inner.recent.iter().cloned().collect(),
        )
    }

    /// Write stats to disk if anything changed.
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let inner = self.lock();
        save_stats(
            &self.state_dir,
            &inner.backend_counts,
            &inner.recent.iter().cloned().collect::<Vec<_>>(),
        );
    }

    /// Persist stats every 500ms until the task is aborted.
    pub async fn run_persistence(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_millis(STATS_PERSIST_INTERVAL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.flush();
        }
    }

    pub fn start_persistence(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(Arc::clone(self).run_persistence())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StatsInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Shared server state.
pub struct AppState {
    pub manager: Arc<BackendManager>,
    pub config: Config,
    pub client: reqwest::Client,
    pub stats: Arc<StatsStore>,
    pub shutting_down: Arc<AtomicBool>,
}

/// A running HTTP server: either the router or a sandbox backend.
pub struct Running {
    addr: SocketAddr,
    server: tokio::task::JoinHandle<()>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    shutting_down: Arc<AtomicBool>,
    on_stop: Option<Box<dyn FnOnce() + Send>>,
}

impl Running {
    /// A bare server with no background tasks or shutdown hooks.
    pub fn new(
        addr: SocketAddr,
        server: tokio::task::JoinHandle<()>,
        shutdown_tx: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        Self {
            addr,
            server,
            shutdown_tx: Some(shutdown_tx),
            tasks: Vec::new(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            on_stop: None,
        }
    }

    /// Track a background task that must be aborted on shutdown.
    pub fn with_task(mut self, task: tokio::task::JoinHandle<()>) -> Self {
        self.tasks.push(task);
        self
    }

    /// Share the router's "shutting down" flag, so `begin_shutdown` makes
    /// in-flight handlers answer 503.
    pub fn with_shutdown_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.shutting_down = flag;
        self
    }

    /// Register state to flush once the drain finishes.
    pub fn on_stop(mut self, hook: impl FnOnce() + Send + 'static) -> Self {
        self.on_stop = Some(Box::new(hook));
        self
    }

    /// The address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Enter shutdown: new requests are answered with 503 from here on.
    /// Returns false when a shutdown was already in progress.
    pub fn begin_shutdown(&self) -> bool {
        !self.shutting_down.swap(true, Ordering::SeqCst)
    }

    /// Stop accepting connections, drain in-flight requests (bounded by
    /// `grace`), abort background tasks and flush persisted state.
    pub async fn stop(mut self, grace: Duration) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = tokio::time::timeout(grace, self.server).await;
        for task in self.tasks {
            task.abort();
        }
        if let Some(hook) = self.on_stop.take() {
            hook();
        }
    }

    /// Stop immediately without draining.
    pub fn abort(&self) {
        self.server.abort();
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Start the router. Binds to `127.0.0.1:<config.port>` (port 0 picks an
/// ephemeral port, which tests use).
pub async fn start_aiproxy(config: Config) -> std::io::Result<Running> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port)).await?;
    let addr = listener.local_addr()?;

    let stats = Arc::new(StatsStore::load(&config));
    let manager = Arc::new(BackendManager::new(config.clone()));
    let client = reqwest::Client::builder()
        .build()
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let shutting_down = Arc::new(AtomicBool::new(false));
    let state = Arc::new(AppState {
        manager: Arc::clone(&manager),
        config,
        client,
        stats: Arc::clone(&stats),
        shutting_down: Arc::clone(&shutting_down),
    });

    // Periodic health checks and debounced persistence of pins and stats.
    let health_checks = BackendManager::start_health_checks(&manager, state.client.clone());
    let pin_persistence = manager.start_persistence();
    let stats_persistence = stats.start_persistence();

    let app: Router = Router::new()
        .fallback(handle_request)
        .with_state(Arc::clone(&state));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        server.await.ok();
    });

    let flush = move || {
        manager.flush();
        stats.flush();
    };
    Ok(Running::new(addr, server, shutdown_tx)
        .with_task(health_checks)
        .with_task(pin_persistence)
        .with_task(stats_persistence)
        .with_shutdown_flag(shutting_down)
        .on_stop(flush))
}

/// Start the router and run until SIGINT/SIGTERM, then drain and flush.
pub async fn run_until_shutdown(config: Config) -> ! {
    let running = match start_aiproxy(config.clone()).await {
        Ok(running) => running,
        Err(err) => {
            error(format!(
                "could not listen on 127.0.0.1:{}: {}",
                config.port, err
            ));
            std::process::exit(1);
        }
    };

    first_shutdown_signal().await;
    if !running.begin_shutdown() {
        std::process::exit(130); // second signal: force exit
    }
    // A further signal during the drain exits immediately.
    tokio::spawn(force_exit_on_signal());

    let grace = Duration::from_millis(config.shutdown_grace_ms);
    info(format!(
        "shutting down; draining in-flight requests (up to {}ms)",
        grace.as_millis()
    ));
    running.stop(grace).await;
    info("shutdown complete");
    std::process::exit(0);
}

/// Wait for the first SIGINT or SIGTERM.
pub async fn first_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match terminate.as_mut() {
                    Some(signal) => { signal.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Exit(130) on any further shutdown signal while draining.
async fn force_exit_on_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => std::process::exit(130),
            _ = async {
                match terminate.as_mut() {
                    Some(signal) => { signal.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => std::process::exit(130),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    }
}

/// Recent-route cap, always at least one entry.
fn max_recent(config: &Config) -> usize {
    (config.recent_routes.max(1)) as usize
}

async fn handle_request(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if state.shutting_down.load(Ordering::SeqCst) {
        return json_error(503, "server is shutting down", "invalid_request_error");
    }

    let (parts, body) = req.into_parts();
    let method = parts.method;
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);
    let t0 = Instant::now();

    let result = dispatch(
        &state,
        &method,
        &path,
        query.as_deref(),
        &parts.headers,
        body,
    )
    .await;

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            error(err.to_string());
            return json_error(500, &err.to_string(), "invalid_request_error");
        }
    };

    let ms = t0.elapsed().as_millis() as u64;

    // Record the routed request, but only when a backend actually served it.
    let entry = RoutingEntry {
        at: to_iso8601(now_ms()),
        session_id: outcome
            .session_key
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        model: outcome.model.clone(),
        // No backend available renders as "none", like the Deno router.
        backend_id: outcome
            .backend_id
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        endpoint: outcome.endpoint.clone().unwrap_or_default(),
        status: outcome.status,
        ms,
    };
    if outcome.backend_id.is_some() {
        state.stats.record(entry.clone(), max_recent(&state.config));
    }
    info(route_log_line(
        &method,
        &path,
        &entry,
        outcome.session_key.as_deref(),
    ));

    let mut builder = Response::builder().status(outcome.status);
    if let Some(headers) = builder.headers_mut() {
        *headers = outcome.headers;
    }
    match builder.body(outcome.body) {
        Ok(response) => response,
        Err(_) => json_error(
            500,
            "failed to build upstream response",
            "invalid_request_error",
        ),
    }
}

/// The per-request log line, e.g.
/// `POST /v1/chat/completions session=f:abc -> b (http://127.0.0.1:1/v1) model=gpt-4o status=200 12ms`.
fn route_log_line(
    method: &Method,
    path: &str,
    entry: &RoutingEntry,
    session_key: Option<&str>,
) -> String {
    let endpoint = if entry.endpoint.is_empty() || entry.backend_id == "none" {
        String::new()
    } else {
        format!(" ({})", entry.endpoint)
    };
    format!(
        "{} {} session={} -> {}{} model={} status={} {}ms",
        method,
        path,
        session_key.unwrap_or("-"),
        entry.backend_id,
        endpoint,
        entry.model,
        entry.status,
        entry.ms,
    )
}

/// A handled request: the response plus the metadata the dashboard records.
struct Handled {
    status: u16,
    headers: HeaderMap,
    body: Body,
    backend_id: Option<String>,
    endpoint: Option<String>,
    session_key: Option<String>,
    model: String,
}

async fn dispatch(
    state: &AppState,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Result<Handled, String> {
    if method == Method::OPTIONS {
        return Ok(Handled {
            status: 204,
            headers: HeaderMap::new(),
            body: Body::empty(),
            backend_id: None,
            endpoint: None,
            session_key: None,
            model: "-".to_string(),
        });
    }

    if method == Method::GET {
        if path == "/" || path == "/index.html" {
            return Ok(Handled {
                status: 200,
                headers: header_map([("content-type", "text/html; charset=utf-8")]),
                body: Body::from(DASHBOARD_HTML),
                backend_id: None,
                endpoint: None,
                session_key: None,
                model: "-".to_string(),
            });
        }
        if path == "/api/status" {
            return Ok(Handled {
                status: 200,
                headers: json_headers(),
                body: Body::from(serde_json::to_vec(&status_json(state)).expect("status json")),
                backend_id: None,
                endpoint: None,
                session_key: None,
                model: "-".to_string(),
            });
        }
        if path == "/models" || path == "/v1/models" {
            if !client_authorized(&state.config, headers) {
                return Ok(json_error_handled(
                    401,
                    "invalid or missing API key",
                    "invalid_request_error",
                ));
            }
            return handle_models(state).await;
        }
        return Ok(json_error_handled(
            404,
            &format!("no route for GET {path}"),
            "invalid_request_error",
        ));
    }

    if method == Method::POST && path.ends_with("/chat/completions") {
        if !client_authorized(&state.config, headers) {
            return Ok(json_error_handled(
                401,
                "invalid or missing API key",
                "invalid_request_error",
            ));
        }

        let bytes = to_bytes(body, usize::MAX)
            .await
            .map_err(|err| format!("could not read request body: {err}"))?;
        let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(json_error_handled(
                400,
                "invalid JSON body",
                "invalid_request_error",
            ));
        };

        let session_key = session_key_from_body(headers, &parsed);
        let ForwardOutcome {
            status,
            headers,
            body,
            backend_id,
            endpoint,
        } = forward_with_retry(
            &state.client,
            ForwardRequest {
                method: Method::POST,
                path,
                query,
                headers,
                body: &parsed,
                session_key: session_key.as_deref(),
            },
            &state.manager,
            &state.config,
        )
        .await;

        let model = match parsed.get("model") {
            Some(model) if !model.is_null() => js_to_string(model),
            _ => "-".to_string(),
        };

        return Ok(Handled {
            status,
            headers,
            body,
            backend_id,
            endpoint,
            session_key,
            model,
        });
    }

    Ok(json_error_handled(
        404,
        &format!("no route for {} {}", method, path),
        "invalid_request_error",
    ))
}

/// `GET /models`: the configured list, a proxied list from a healthy backend,
/// or a minimal default.
async fn handle_models(state: &AppState) -> Result<Handled, String> {
    if let Some(models) = &state.config.models {
        return Ok(Handled {
            status: 200,
            headers: json_headers(),
            body: Body::from(serde_json::to_vec(&models_json(models)).expect("models json")),
            backend_id: None,
            endpoint: None,
            session_key: None,
            model: "-".to_string(),
        });
    }

    if let Some(chosen) = state.manager.pick(None, &std::collections::HashSet::new()) {
        let url = with_path(&chosen.base_url, "/models");
        if let Ok(upstream) = state
            .client
            .get(&url)
            .timeout(Duration::from_millis(MODELS_PROXY_TIMEOUT_MS))
            .send()
            .await
        {
            if upstream.status().is_success() {
                return Ok(Handled {
                    status: upstream.status().as_u16(),
                    headers: upstream.headers().clone(),
                    body: Body::from_stream(upstream.bytes_stream()),
                    backend_id: None,
                    endpoint: None,
                    session_key: None,
                    model: "-".to_string(),
                });
            }
        }
    }

    Ok(Handled {
        status: 200,
        headers: json_headers(),
        body: Body::from(serde_json::to_vec(&models_json(&["gpt-4o"])).expect("models json")),
        backend_id: None,
        endpoint: None,
        session_key: None,
        model: "-".to_string(),
    })
}

fn models_json(models: &[impl AsRef<str>]) -> Value {
    json!({
        "object": "list",
        "data": models.iter().map(|id| json!({
            "id": id.as_ref(),
            "object": "model",
            "owned_by": "aiproxy",
        })).collect::<Vec<_>>(),
    })
}

fn status_json(state: &AppState) -> Value {
    let (backend_counts, recent) = state.stats.snapshot();
    let max_recent = max_recent(&state.config);
    let backends: Vec<Value> = state
        .manager
        .health_report()
        .iter()
        .map(|backend| {
            let mut entry = json!({
                "id": backend.id,
                "baseUrl": backend.base_url,
                "healthy": backend.healthy,
                "requests": backend_counts.get(&backend.id).copied().unwrap_or(0),
            });
            if let Some(priority) = backend.priority {
                entry["priority"] = json!(priority);
            }
            entry
        })
        .collect();

    json!({
        "service": "aiproxy",
        "config": {
            "port": state.config.port,
            "sticky": state.config.sticky,
            "retryStatusCodes": state.config.retry_codes(),
            "retryOnLimitMessage": state.config.retry_on_limit_message,
            "authRequired": state.config.auth_required(),
            "recentRoutes": max_recent,
            "shutdownGraceMs": state.config.shutdown_grace_ms,
        },
        "models": state.config.models,
        "backends": backends,
        "recent": recent.iter().take(max_recent).collect::<Vec<_>>(),
    })
}

/// True when client auth is disabled or the request carries a valid key.
fn client_authorized(config: &Config, headers: &HeaderMap) -> bool {
    let Some(keys) = config.api_keys.as_ref() else {
        return true;
    };
    if keys.is_empty() {
        return true;
    }
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    match bearer_token(value) {
        Some(token) => keys.iter().any(|key| key == token),
        None => false,
    }
}

/// Extract the token from a `Bearer <key>` header, case-insensitively.
fn bearer_token(value: &str) -> Option<&str> {
    let split = value.find(char::is_whitespace).unwrap_or(value.len());
    let (scheme, rest) = value.split_at(split);
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        return None;
    }
    Some(token)
}

fn header_map<const N: usize>(pairs: [(&str, &str); N]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

fn json_headers() -> HeaderMap {
    header_map([("content-type", "application/json")])
}

fn json_body(value: &Value) -> Body {
    Body::from(serde_json::to_vec(value).expect("json body"))
}

fn json_error_handled(status: u16, message: &str, error_type: &str) -> Handled {
    Handled {
        status,
        headers: json_headers(),
        body: json_body(&error_json(status, message, error_type)),
        backend_id: None,
        endpoint: None,
        session_key: None,
        model: "-".to_string(),
    }
}

fn error_json(status: u16, message: &str, error_type: &str) -> Value {
    json!({
        "error": {
            "message": message,
            "type": error_type,
            "code": status,
        }
    })
}

fn json_error(status: u16, message: &str, error_type: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        json_body(&error_json(status.as_u16(), message, error_type)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![
                crate::types::BackendConfig::new("a", "http://127.0.0.1:1/v1"),
                crate::types::BackendConfig::new("b", "http://127.0.0.1:2/v1"),
            ],
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        (config, dir)
    }

    #[test]
    fn stats_record_counts_and_caps_recent_routes() {
        let (config, _dir) = test_config();
        let stats = StatsStore::load(&config);
        let entry = |id: u64| RoutingEntry {
            at: format!("2026-01-01T00:00:{id:02}.000Z"),
            session_id: format!("f:{id}"),
            model: "gpt-4o".to_string(),
            backend_id: "a".to_string(),
            endpoint: "http://127.0.0.1:1/v1".to_string(),
            status: 200,
            ms: 1,
        };
        for id in 0..5 {
            stats.record(entry(id), 200);
        }
        let (counts, recent) = stats.snapshot();
        assert_eq!(counts.get("a"), Some(&5));
        assert_eq!(counts.get("b"), Some(&0));
        assert_eq!(recent.len(), 5);
        // Newest first.
        assert_eq!(recent[0].session_id, "f:4");
        assert_eq!(recent[4].session_id, "f:0");
    }

    #[test]
    fn stats_cap_keeps_the_newest_entries() {
        let (mut config, _dir) = test_config();
        config.recent_routes = 2;
        let stats = StatsStore::load(&config);
        let entry = |id: u64| RoutingEntry {
            at: format!("2026-01-01T00:00:{id:02}.000Z"),
            session_id: format!("f:{id}"),
            model: "gpt-4o".to_string(),
            backend_id: "a".to_string(),
            endpoint: "http://127.0.0.1:1/v1".to_string(),
            status: 200,
            ms: 1,
        };
        for id in 0..5 {
            stats.record(entry(id), max_recent(&config));
        }
        let (_, recent) = stats.snapshot();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].session_id, "f:4");
        assert_eq!(recent[1].session_id, "f:3");
    }

    #[test]
    fn max_recent_is_at_least_one() {
        let (mut config, _dir) = test_config();
        config.recent_routes = 0;
        assert_eq!(max_recent(&config), 1);
    }

    #[test]
    fn stats_restore_filters_unknown_backends_and_caps() {
        let (mut config, dir) = test_config();
        config.recent_routes = 2;
        let state_dir = dir.path().display().to_string();

        let mut counts = BTreeMap::new();
        counts.insert("a".to_string(), 7);
        counts.insert("b".to_string(), 0);
        counts.insert("gone".to_string(), 9);
        let recent = vec![
            RoutingEntry {
                at: "2026-01-01T00:00:01.000Z".to_string(),
                session_id: "f:1".to_string(),
                model: "gpt-4o".to_string(),
                backend_id: "a".to_string(),
                endpoint: "http://127.0.0.1:1/v1".to_string(),
                status: 200,
                ms: 1,
            },
            RoutingEntry {
                at: "2026-01-01T00:00:02.000Z".to_string(),
                session_id: "f:2".to_string(),
                model: "gpt-4o".to_string(),
                backend_id: "gone".to_string(),
                endpoint: "http://127.0.0.1:9/v1".to_string(),
                status: 200,
                ms: 2,
            },
            RoutingEntry {
                at: "2026-01-01T00:00:03.000Z".to_string(),
                session_id: "f:3".to_string(),
                model: "gpt-4o".to_string(),
                backend_id: "b".to_string(),
                endpoint: "http://127.0.0.1:2/v1".to_string(),
                status: 200,
                ms: 3,
            },
        ];
        save_stats(std::path::Path::new(&state_dir), &counts, &recent);

        let stats = StatsStore::load(&config);
        let (restored_counts, restored_recent) = stats.snapshot();
        assert_eq!(restored_counts.get("a"), Some(&7));
        assert_eq!(restored_counts.get("gone"), None);
        assert_eq!(restored_recent.len(), 2);
        assert_eq!(restored_recent[0].session_id, "f:1");
        assert_eq!(restored_recent[1].session_id, "f:3");
    }

    #[test]
    fn stats_persist_only_writes_when_dirty() {
        let (config, dir) = test_config();
        let stats = StatsStore::load(&config);
        stats.flush();
        assert!(!dir.path().join("stats.json").exists());

        stats.record(
            RoutingEntry {
                at: "2026-01-01T00:00:00.000Z".to_string(),
                session_id: "f:1".to_string(),
                model: "gpt-4o".to_string(),
                backend_id: "a".to_string(),
                endpoint: "http://127.0.0.1:1/v1".to_string(),
                status: 200,
                ms: 1,
            },
            200,
        );
        stats.flush();
        assert!(dir.path().join("stats.json").exists());
    }

    #[test]
    fn bearer_token_extraction_matches_the_deno_regex() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER   abc  "), Some("abc"));
        assert_eq!(bearer_token("Token abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer a b"), Some("a b"));
        assert_eq!(bearer_token(""), None);
    }

    #[test]
    fn client_authorization_rules() {
        let config = Config {
            api_keys: Some(vec!["k1".to_string()]),
            ..Config::default()
        };
        let valid = header_map([("authorization", "Bearer k1")]);
        let wrong = header_map([("authorization", "Bearer nope")]);
        let malformed = header_map([("authorization", "k1")]);
        assert!(client_authorized(&config, &valid));
        assert!(!client_authorized(&config, &wrong));
        assert!(!client_authorized(&config, &malformed));
        assert!(!client_authorized(&config, &HeaderMap::new()));

        // No keys configured: everything is authorized.
        let open = Config::default();
        assert!(client_authorized(&open, &HeaderMap::new()));
        let empty = Config {
            api_keys: Some(vec![]),
            ..Config::default()
        };
        assert!(client_authorized(&empty, &HeaderMap::new()));
    }

    #[test]
    fn models_json_advertises_the_expected_shape() {
        let value = models_json(&["gpt-4o", "claude"]);
        assert_eq!(value["object"], "list");
        assert_eq!(value["data"][0]["id"], "gpt-4o");
        assert_eq!(value["data"][0]["object"], "model");
        assert_eq!(value["data"][0]["owned_by"], "aiproxy");
        assert_eq!(value["data"].as_array().expect("array").len(), 2);
    }

    #[test]
    fn error_json_matches_the_openai_shape() {
        let value = error_json(401, "invalid or missing API key", "invalid_request_error");
        assert_eq!(value["error"]["message"], "invalid or missing API key");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(value["error"]["code"], 401);
    }

    #[test]
    fn route_log_line_matches_the_deno_format() {
        let entry = RoutingEntry {
            at: "2026-01-01T00:00:00.000Z".to_string(),
            session_id: "f:abc".to_string(),
            model: "gpt-4o".to_string(),
            backend_id: "b".to_string(),
            endpoint: "http://127.0.0.1:1/v1".to_string(),
            status: 200,
            ms: 12,
        };
        assert_eq!(
            route_log_line(
                &Method::POST,
                "/v1/chat/completions",
                &entry,
                Some("f:abc")
            ),
            "POST /v1/chat/completions session=f:abc -> b (http://127.0.0.1:1/v1) model=gpt-4o status=200 12ms"
        );
        // No session key and no endpoint render as "-" and no suffix.
        assert_eq!(
            route_log_line(&Method::GET, "/v1/models", &entry, None)
                .split(" -> ")
                .next()
                .expect("prefix"),
            "GET /v1/models session=-"
        );
        // A request no backend could serve logs "none" with no endpoint.
        let unserved = RoutingEntry {
            backend_id: "none".to_string(),
            endpoint: String::new(),
            status: 502,
            ..entry
        };
        assert_eq!(
            route_log_line(
                &Method::POST,
                "/v1/chat/completions",
                &unserved,
                Some("f:abc")
            ),
            "POST /v1/chat/completions session=f:abc -> none model=gpt-4o status=502 12ms"
        );
    }

    #[test]
    fn dashboard_html_is_served_verbatim() {
        assert!(DASHBOARD_HTML.starts_with("<!DOCTYPE html>"));
        assert!(DASHBOARD_HTML.contains("aiproxy dashboard"));
        assert!(DASHBOARD_HTML.contains("/api/status"));
    }
}
