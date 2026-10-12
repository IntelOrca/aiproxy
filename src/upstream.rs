//! Upstream forwarding: header rewriting, retry/failover and streaming
//! passthrough.

use crate::backend::{with_path, BackendChoice, BackendManager};
use crate::session::upstream_session_value;
use crate::types::{BackendConfig, Config};
use axum::body::Body;
use http::{HeaderMap, Method};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

/// Headers that must not be forwarded in either direction.
const HOP_BY_HOP: [&str; 11] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "content-encoding",
];

/// The outcome of forwarding a chat completion, ready to be relayed to the
/// client.
pub struct ForwardOutcome {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Body,
    /// The backend that served the request, or None when none was available.
    pub backend_id: Option<String>,
    /// Base URL of the backend that served the request.
    pub endpoint: Option<String>,
}

/// Rate-limit wordings that trigger same-backend retries. Funds/credit/quota
/// wordings are deliberately *not* here: those fail over immediately (see
/// [`FUNDS_RE`]).
static LIMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)rate\s*limit|rate_limit|limit\s*reached|reached.{0,12}limit|too many requests|overloaded")
        .expect("rate limit regex")
});

/// Funds/credit/quota exhaustion — the account cannot serve this backend
/// right now, so waiting will not help: fail over immediately.
static FUNDS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)insufficient\s+quota|insufficient\s+(?:funds|balance|credits?)|credits?error|payment\s+required|out\s+of\s+credits|no\s+credits|billing|quota",
    )
    .expect("funds regex")
});

/// Delay before the nth (0-based) retry of the same backend: `base_ms`
/// doubled each time, capped at 60s. A base of 0 disables the waits (tests).
pub fn retry_delay_ms(base_ms: u64, retry: u64) -> u64 {
    const MAX_MS: u64 = 60_000;
    base_ms.saturating_mul(1u64 << retry.min(32)).min(MAX_MS)
}

/// Whether the `x-opencode-session` header should be sent to this backend.
/// URL-only: sent when the backend baseUrl's hostname is opencode.ai
/// (including subdomains, e.g. the /zen/ gateways).
pub fn should_send_opencode_session(base_url: &str) -> bool {
    match url::Url::parse(base_url) {
        Ok(url) => match url.host_str() {
            Some(host) => {
                let host = host.to_ascii_lowercase();
                host == "opencode.ai" || host.ends_with(".opencode.ai")
            }
            None => false,
        },
        Err(_) => false,
    }
}

/// Compute the conversation-ID headers to send upstream. Derived from the
/// router's internal session key so header-less clients (e.g. Copilot CLI,
/// fingerprinted as `f:…`) still get sticky caching upstream — and pass
/// OpenCode Go's `x-opencode-session` enforcement. Client-provided values
/// are preserved; only missing headers are filled in. Header-only on
/// purpose: no request-body mutation.
pub fn upstream_session_headers(
    base_url: &str,
    session_key: Option<&str>,
    headers: &mut HeaderMap,
) {
    let Some(session_value) = upstream_session_value(session_key) else {
        return;
    };
    if !headers.contains_key("x-session-id") {
        headers.insert("x-session-id", header_value(&session_value));
    }
    if !headers.contains_key("x-opencode-session") && should_send_opencode_session(base_url) {
        headers.insert("x-opencode-session", header_value(&session_value));
    }
}

fn header_value(value: &str) -> http::HeaderValue {
    http::HeaderValue::from_str(value).unwrap_or_else(|_| http::HeaderValue::from_static(""))
}

/// The incoming request, as far as forwarding is concerned.
pub struct ForwardRequest<'a> {
    pub method: Method,
    /// Path of the incoming request (e.g. `/v1/chat/completions`).
    pub path: &'a str,
    /// Query string of the incoming request, without the `?`.
    pub query: Option<&'a str>,
    pub headers: &'a HeaderMap,
    pub body: &'a serde_json::Value,
    pub session_key: Option<&'a str>,
}

/// Forward the request to a backend.
///
/// Each backend gets its own retry budget (`retries`, default 5) with a growing
/// wait between attempts (5s doubling to a 60s cap by default). Retries cover
/// rate-limit verdicts, upstream 5xx responses and transport errors; funds,
/// credit and quota failures fail over immediately instead of waiting.
///
/// When a backend's retries are exhausted the request moves on to the next
/// backend (up to `maxRetries` extra backends). The session is pinned only to
/// a backend that actually returned a success, so failed responses cannot
/// strand a conversation on a backend that cannot serve it. If every backend
/// fails, a funds failure is reported to the client when one was seen;
/// otherwise the last rate-limit/server response is relayed, falling back to a
/// 502 when nothing responded.
pub async fn forward_with_retry(
    client: &reqwest::Client,
    request: ForwardRequest<'_>,
    manager: &BackendManager,
    config: &Config,
) -> ForwardOutcome {
    let max_backends = (config.max_retries + 1).max(1) as usize;
    let mut tried: HashSet<String> = HashSet::new();
    let retry_codes: HashSet<u16> = config.retry_codes().into_iter().collect();
    let scan_body = config.retry_on_limit_message;
    let session_key = request.session_key;
    let mut funds_failure: Option<ForwardOutcome> = None;
    let mut last_failure: Option<ForwardOutcome> = None;

    'backends: for _ in 0..max_backends {
        let Some(chosen) = manager.pick(session_key, &tried) else {
            break;
        };
        tried.insert(chosen.id.clone());

        let mut retry: u64 = 0;
        loop {
            let response = match forward_once(client, &request, &chosen, config).await {
                Ok(response) => response,
                Err(err) => {
                    crate::log::warn(format!("backend {} failed: {}", chosen.id, err));
                    if retry < chosen.retries {
                        let delay = retry_delay_ms(config.retry_base_delay_ms, retry);
                        crate::log::warn(format!(
                            "retrying backend {} in {}ms ({}/{})",
                            chosen.id,
                            delay,
                            retry + 1,
                            chosen.retries
                        ));
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        retry += 1;
                        // The health checker may have confirmed the backend is
                        // down while we waited — fail over instead of burning
                        // the rest of the retry budget on a dead backend.
                        if manager.is_healthy(&chosen.id) {
                            continue;
                        }
                    }
                    manager.mark_down(&chosen.id);
                    continue 'backends;
                }
            };

            match classify(response, &retry_codes, scan_body, &chosen).await {
                Verdict::Forward(outcome) => {
                    if (200..300).contains(&outcome.status) {
                        if let Some(key) = session_key {
                            manager.pin_to(key, &chosen.id);
                        }
                    }
                    return outcome;
                }
                Verdict::Funds(outcome) => {
                    // Out of funds: waiting will not help — try the next
                    // backend, and report this only if every backend fails.
                    crate::log::warn(format!(
                        "backend {} reports insufficient funds ({}); failing over",
                        chosen.id, outcome.status
                    ));
                    funds_failure = Some(outcome);
                    continue 'backends;
                }
                Verdict::Limit(outcome) => {
                    if retry < chosen.retries {
                        let delay = retry_delay_ms(config.retry_base_delay_ms, retry);
                        crate::log::warn(format!(
                            "backend {} limit reached; retrying in {}ms ({}/{})",
                            chosen.id,
                            delay,
                            retry + 1,
                            chosen.retries
                        ));
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        retry += 1;
                        continue;
                    }
                    crate::log::warn(format!(
                        "backend {} limit reached after {} retries; failing over",
                        chosen.id, retry
                    ));
                    last_failure = Some(outcome);
                    continue 'backends;
                }
                Verdict::ServerError(outcome) => {
                    if retry < chosen.retries {
                        let delay = retry_delay_ms(config.retry_base_delay_ms, retry);
                        crate::log::warn(format!(
                            "backend {} server error ({}); retrying in {}ms ({}/{})",
                            chosen.id,
                            outcome.status,
                            delay,
                            retry + 1,
                            chosen.retries
                        ));
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        retry += 1;
                        continue;
                    }
                    crate::log::warn(format!(
                        "backend {} server error ({}) after {} retries; failing over",
                        chosen.id, outcome.status, retry
                    ));
                    last_failure = Some(outcome);
                    continue 'backends;
                }
            }
        }
    }

    if let Some(outcome) = funds_failure.or(last_failure) {
        return outcome;
    }

    ForwardOutcome {
        status: 502,
        headers: HeaderMap::new(),
        body: Body::from(
            r#"{"error":{"message":"no healthy backends available","type":"server_error","code":502}}"#
                .to_string(),
        ),
        backend_id: None,
        endpoint: None,
    }
}

/// The classification of an upstream response.
enum Verdict {
    /// Relay this response as-is: a success, or a non-retryable client error.
    Forward(ForwardOutcome),
    /// Rate limited — retry the same backend, then fail over.
    Limit(ForwardOutcome),
    /// Funds/credit/quota exhausted — fail over immediately without retrying.
    Funds(ForwardOutcome),
    /// Upstream server error (5xx) — retry the same backend, then fail over.
    ServerError(ForwardOutcome),
}

/// Build the relay outcome from a streaming upstream response.
fn outcome_from_stream(response: reqwest::Response, chosen: BackendChoice) -> ForwardOutcome {
    let status = response.status().as_u16();
    let headers = filter_response_headers(response.headers());
    ForwardOutcome {
        status,
        headers,
        body: Body::from_stream(response.bytes_stream()),
        backend_id: Some(chosen.id),
        endpoint: Some(chosen.base_url),
    }
}

/// Classify an upstream response. Successes keep streaming; error bodies are
/// buffered so they can be inspected (and later relayed if nothing else
/// serves the request).
async fn classify(
    response: reqwest::Response,
    codes: &HashSet<u16>,
    scan_body: bool,
    chosen: &BackendConfig,
) -> Verdict {
    let status = response.status().as_u16();
    if status < 400 {
        return Verdict::Forward(outcome_from_stream(response, chosen.clone()));
    }

    let status_code = response.status();
    let headers = filter_response_headers(response.headers());
    let bytes = response.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let outcome = outcome_from_buffered(status_code, headers, bytes, chosen.clone());

    // Funds/credit/quota failures are not worth waiting on. A 402 is always
    // one; other statuses are decided by their wording.
    if status == 402 || FUNDS_RE.is_match(&text) {
        return Verdict::Funds(outcome);
    }
    if codes.contains(&status) || (scan_body && LIMIT_RE.is_match(&text)) {
        return Verdict::Limit(outcome);
    }
    if (500..600).contains(&status) {
        return Verdict::ServerError(outcome);
    }
    // Everything else is a request-level error (bad model, malformed body):
    // relay it as-is rather than retrying something that cannot heal.
    Verdict::Forward(outcome)
}

/// Relay one response after its body has been buffered for inspection.
fn outcome_from_buffered(
    status: http::StatusCode,
    headers: HeaderMap,
    bytes: bytes::Bytes,
    chosen: BackendChoice,
) -> ForwardOutcome {
    ForwardOutcome {
        status: status.as_u16(),
        headers,
        body: Body::from(bytes),
        backend_id: Some(chosen.id),
        endpoint: Some(chosen.base_url),
    }
}

/// Send the request to one backend, rewriting headers and the model name.
async fn forward_once(
    client: &reqwest::Client,
    request: &ForwardRequest<'_>,
    backend: &BackendConfig,
    config: &Config,
) -> Result<reqwest::Response, reqwest::Error> {
    // Normalize: map any client route suffix onto the backend's API root.
    let target_path = if request.path.ends_with("/models") {
        "/models"
    } else if request.path.ends_with("/chat/completions") {
        "/chat/completions"
    } else {
        request.path
    };
    let target = format!(
        "{}{}",
        with_path(&backend.base_url, target_path),
        request.query.map(|q| format!("?{q}")).unwrap_or_default(),
    );

    let mut headers = request.headers.clone();
    for hop in HOP_BY_HOP {
        headers.remove(hop);
    }
    headers.insert("host", header_value(&backend_host(&backend.base_url)));

    // When the router has its own client keys, the client's key is the
    // router's credential — don't leak it upstream. A per-backend apiKey (if
    // any) then provides the upstream Authorization header instead.
    if config.auth_required() {
        headers.remove("authorization");
    }
    if let Some(api_key) = &backend.api_key {
        headers.insert("authorization", header_value(&format!("Bearer {api_key}")));
    }

    // Conversation-ID headers for upstream prompt-cache affinity (see
    // upstream_session_headers). Injected here so every forward — including
    // retries/failovers — carries them.
    upstream_session_headers(&backend.base_url, request.session_key, &mut headers);

    // Optional model rewrite: per-backend override wins, then global map.
    let body = rewrite_model(request.body, backend, config);

    client
        .request(request.method.clone(), &target)
        .headers(headers)
        .body(serde_json::to_vec(&body).unwrap_or_default())
        .timeout(Duration::from_millis(config.upstream_timeout_ms))
        .send()
        .await
}

/// Apply the per-backend model override, then the global model map.
fn rewrite_model(
    body: &serde_json::Value,
    backend: &BackendConfig,
    config: &Config,
) -> serde_json::Value {
    let serde_json::Value::Object(_) = body else {
        return body.clone();
    };
    let Some(serde_json::Value::String(model)) = body.get("model") else {
        return body.clone();
    };
    let mapped = backend.model.clone().or_else(|| {
        config
            .model_map
            .as_ref()
            .and_then(|m| m.get(model).cloned())
    });
    match mapped {
        Some(mapped) => {
            let mut rewritten = body.clone();
            if let Some(obj) = rewritten.as_object_mut() {
                obj.insert("model".to_string(), serde_json::Value::String(mapped));
            }
            rewritten
        }
        None => body.clone(),
    }
}

/// The `host` header value for a backend base URL (port included, as the
/// Deno version used `new URL(baseUrl).host`).
fn backend_host(base_url: &str) -> String {
    match url::Url::parse(base_url) {
        Ok(url) => {
            let host = url.host_str().unwrap_or_default().to_string();
            match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        }
        Err(_) => base_url.to_string(),
    }
}

/// Copy upstream response headers, minus hop-by-hop headers.
fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendManager;
    use crate::testing;
    use crate::types::BackendConfig;
    use axum::body::to_bytes;
    use axum::http::header::{AUTHORIZATION, HOST};
    use axum::routing::post;
    use http::HeaderName;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn chat_headers() -> HeaderMap {
        header_map(&[("content-type", "application/json")])
    }

    fn chat_body() -> serde_json::Value {
        serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        })
    }

    fn backend(id: &str, port: u16) -> BackendConfig {
        BackendConfig {
            id: id.to_string(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            weight: 1.0,
            model: None,
            api_key: None,
            priority: None,
            // Tests stay fast and predictable; retry tests opt in per backend.
            retries: 0,
        }
    }

    /// A two-backend config: `primary` (priority 0) and `backup` (priority 1).
    struct Setup {
        client: reqwest::Client,
        config: Config,
        manager: BackendManager,
        _dir: tempfile::TempDir,
    }

    fn setup(primary_port: u16, backup_port: u16) -> Setup {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![
                BackendConfig {
                    priority: Some(0),
                    ..backend("primary", primary_port)
                },
                BackendConfig {
                    priority: Some(1),
                    ..backend("backup", backup_port)
                },
            ],
            health_check_interval_ms: 60_000,
            health_check_timeout_ms: 1_000,
            session_ttl_ms: 60_000,
            upstream_timeout_ms: 5_000,
            // No waits between retries in tests.
            retry_base_delay_ms: 0,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        Setup {
            client: reqwest::Client::new(),
            manager: BackendManager::new(config.clone()),
            config,
            _dir: dir,
        }
    }

    /// Forward a chat completion, mirroring the Deno tests' `chatRequest`.
    async fn forward(
        setup: &Setup,
        body: &serde_json::Value,
        session_key: Option<&str>,
    ) -> ForwardOutcome {
        forward_with_retry(
            &setup.client,
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body,
                session_key,
            },
            &setup.manager,
            &setup.config,
        )
        .await
    }

    async fn body_text(outcome: &mut ForwardOutcome) -> String {
        let body = std::mem::replace(&mut outcome.body, Body::from(String::new()));
        String::from_utf8_lossy(&to_bytes(body, usize::MAX).await.expect("body")).to_string()
    }

    #[tokio::test]
    async fn rate_limit_429_fails_over_to_backup_without_marking_primary_down() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(serde_json::json!({"error": {"message": "rate limit"}})),
                        )
                    }
                }),
            ))
            .await
        };
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let setup = setup(primary.port, backup.port);
        let mut outcome = forward(&setup, &chat_body(), Some("f:abc")).await;

        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 200);
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
        // A rate limit is not "down" — the primary stays healthy.
        assert!(
            setup
                .manager
                .health_report()
                .iter()
                .find(|b| b.id == "primary")
                .expect("primary")
                .healthy
        );
        assert_eq!(body_text(&mut outcome).await, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn rate_limit_failover_re_pins_the_session_to_the_backend_that_served() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::TOO_MANY_REQUESTS
                    }
                }),
            ))
            .await
        };
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let setup = setup(primary.port, backup.port);
        let body = serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "turn"}]});

        // Turn 1: primary 429 -> fail over to backup.
        let r1 = forward(&setup, &body, Some("f:keep")).await;
        assert_eq!(r1.backend_id.as_deref(), Some("backup"));
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 1);

        // Turn 2: same session is now pinned to the backup (cache affinity) —
        // the primary must NOT be re-tried.
        let r2 = forward(&setup, &body, Some("f:keep")).await;
        assert_eq!(r2.backend_id.as_deref(), Some("backup"));
        assert_eq!(
            primary_hits.load(Ordering::SeqCst),
            1,
            "primary must not be retried once the session was pinned away"
        );
        assert_eq!(backup_hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn new_sessions_still_try_the_primary_first_then_pin_to_the_serving_fallback() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::TOO_MANY_REQUESTS
                    }
                }),
            ))
            .await
        };
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let setup = setup(primary.port, backup.port);
        let body_a = serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "session a"}]});

        // Session A rate-limits on the primary and is pinned to the backup.
        forward(&setup, &body_a, Some("f:a")).await;
        forward(&setup, &body_a, Some("f:a")).await;
        assert_eq!(
            primary_hits.load(Ordering::SeqCst),
            1,
            "session a stays on backup after failover"
        );

        // Session B (new) still tries the primary first, then fails over.
        let body_b = serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "session b"}]});
        forward(&setup, &body_b, Some("f:b")).await;
        assert_eq!(
            primary_hits.load(Ordering::SeqCst),
            2,
            "a fresh session still tries the primary first"
        );
        assert_eq!(backup_hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn rate_limit_message_in_error_body_triggers_failover() {
        let primary = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": {"message": "you have reached your limit"}
                    })),
                )
            }),
        ))
        .await;
        let backup = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        ))
        .await;

        let setup = setup(primary.port, backup.port);
        let outcome = forward(&setup, &chat_body(), Some("f:x")).await;
        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 200);
    }

    #[tokio::test]
    async fn server_errors_are_retried_on_the_same_backend_then_fail_over() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"error": {"message": "boom"}})),
                        )
                    }
                }),
            ))
            .await
        };
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let mut setup = setup(primary.port, backup.port);
        // Two retries: three attempts on the primary before failing over.
        setup.config.backends[0].retries = 2;
        setup.manager = BackendManager::new(setup.config.clone());
        let outcome = forward(&setup, &chat_body(), Some("f:x")).await;

        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 200);
        assert_eq!(primary_hits.load(Ordering::SeqCst), 3);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn client_errors_are_relayed_without_retrying() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            axum::Json(
                                serde_json::json!({"error": {"message": "model not found"}}),
                            ),
                        )
                    }
                }),
            ))
            .await
        };
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let mut setup = setup(primary.port, backup.port);
        setup.config.backends[0].retries = 3;
        setup.manager = BackendManager::new(setup.config.clone());
        let mut outcome = forward(&setup, &chat_body(), Some("f:x")).await;

        assert_eq!(outcome.backend_id.as_deref(), Some("primary"));
        assert_eq!(outcome.status, 400);
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
        assert!(body_text(&mut outcome).await.contains("model not found"));
    }

    #[tokio::test]
    async fn funds_failure_fails_over_immediately_without_retrying() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::PAYMENT_REQUIRED,
                            axum::Json(serde_json::json!({
                                "error": {"message": "Insufficient balance."}
                            })),
                        )
                    }
                }),
            ))
            .await
        };
        let backup = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        ))
        .await;

        let mut setup = setup(primary.port, backup.port);
        // Even with a big retry budget, funds failures must not be retried.
        setup.config.backends[0].retries = 3;
        setup.manager = BackendManager::new(setup.config.clone());
        let outcome = forward(&setup, &chat_body(), Some("f:pay")).await;

        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 200);
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
        // The session follows the backend that actually served it.
        assert_eq!(
            setup.manager.pin_backend_id("f:pay").as_deref(),
            Some("backup")
        );
    }

    #[tokio::test]
    async fn credits_error_on_a_401_is_treated_as_a_funds_failure() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::UNAUTHORIZED,
                            axum::Json(serde_json::json!({
                                "type": "CreditsError",
                                "message": "Insufficient balance. Manage your billing here."
                            })),
                        )
                    }
                }),
            ))
            .await
        };
        let backup = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        ))
        .await;

        let mut setup = setup(primary.port, backup.port);
        setup.config.backends[0].retries = 3;
        setup.manager = BackendManager::new(setup.config.clone());
        let outcome = forward(&setup, &chat_body(), Some("f:pay")).await;

        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 200);
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn funds_failure_is_reported_when_every_backend_fails() {
        fn funds_backend() -> axum::Router {
            axum::Router::new().route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        axum::http::StatusCode::PAYMENT_REQUIRED,
                        axum::Json(serde_json::json!({
                            "error": {"message": "Insufficient balance."}
                        })),
                    )
                }),
            )
        }
        let primary = testing::spawn_test_server(funds_backend()).await;
        let backup = testing::spawn_test_server(funds_backend()).await;

        let setup = setup(primary.port, backup.port);
        let mut outcome = forward(&setup, &chat_body(), Some("f:pay")).await;

        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 402);
        assert!(body_text(&mut outcome)
            .await
            .contains("Insufficient balance"));
    }

    #[tokio::test]
    async fn failed_responses_do_not_pin_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::PAYMENT_REQUIRED,
                    axum::Json(serde_json::json!({"error": {"message": "Insufficient balance."}})),
                )
            }),
        ))
        .await;
        let config = Config {
            backends: vec![backend("only", server.port)],
            sticky: true,
            retry_base_delay_ms: 0,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());

        let outcome = forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: Some("f:pay"),
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(outcome.status, 402);
        assert_eq!(
            manager.pin_backend_id("f:pay"),
            None,
            "an error response must not pin the session"
        );
    }

    #[tokio::test]
    async fn all_backends_limited_returns_the_limit_response() {
        fn limited_backend() -> axum::Router {
            axum::Router::new().route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(serde_json::json!({"error": {"message": "rate limit"}})),
                    )
                }),
            )
        }
        let primary = testing::spawn_test_server(limited_backend()).await;
        let backup = testing::spawn_test_server(limited_backend()).await;

        let setup = setup(primary.port, backup.port);
        let mut outcome = forward(&setup, &chat_body(), Some("f:x")).await;

        // The last backend's limit response is relayed rather than a 502.
        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 429);
        assert!(body_text(&mut outcome).await.contains("rate limit"));
    }

    #[tokio::test]
    async fn retry_on_limit_message_false_disables_body_scanning() {
        let primary = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(
                        serde_json::json!({"error": {"message": "you have reached your limit"}}),
                    ),
                )
            }),
        ))
        .await;
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let mut setup = setup(primary.port, backup.port);
        setup.config.retry_on_limit_message = false;
        let outcome = forward(&setup, &chat_body(), Some("f:x")).await;
        assert_eq!(outcome.backend_id.as_deref(), Some("primary"));
        assert_eq!(outcome.status, 400);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retry_status_codes_are_configurable() {
        let primary = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
        ))
        .await;
        let backup = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        ))
        .await;

        let mut setup = setup(primary.port, backup.port);
        // 429 is not in the retry list, so the primary's response is returned.
        setup.config.retry_status_codes = Some(vec![503]);
        let outcome = forward(&setup, &chat_body(), Some("f:x")).await;
        assert_eq!(outcome.backend_id.as_deref(), Some("primary"));
        assert_eq!(outcome.status, 429);
    }

    #[tokio::test]
    async fn max_retries_zero_means_a_single_attempt() {
        let primary = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
        ))
        .await;
        let backup_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let backup = {
            let hits = backup_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };

        let mut setup = setup(primary.port, backup.port);
        setup.config.max_retries = 0;
        let outcome = forward(&setup, &chat_body(), Some("f:x")).await;
        assert_eq!(outcome.backend_id.as_deref(), Some("primary"));
        assert_eq!(outcome.status, 429);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn healthy_primary_backend_is_used_first() {
        let primary_hits = std::sync::Arc::new(AtomicUsize::new(0));
        let primary = {
            let hits = primary_hits.clone();
            testing::spawn_test_server(axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            ))
            .await
        };
        let backup = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        ))
        .await;

        let setup = setup(primary.port, backup.port);
        let outcome = forward(&setup, &chat_body(), Some("f:y")).await;
        assert_eq!(outcome.backend_id.as_deref(), Some("primary"));
        assert_eq!(primary_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transport_error_marks_the_backend_down_and_fails_over() {
        // Port 1 has nothing listening: the request fails at connect time.
        let backup = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        ))
        .await;
        let mut setup = setup(1, backup.port);
        setup.config.backends[1].base_url = backup.url("/v1");
        // A retry is attempted before the backend is given up on.
        setup.config.backends[0].retries = 1;
        let setup = Setup {
            client: setup.client,
            manager: BackendManager::new(setup.config.clone()),
            config: setup.config,
            _dir: setup._dir,
        };

        let outcome = forward(&setup, &chat_body(), Some("f:err")).await;
        assert_eq!(outcome.backend_id.as_deref(), Some("backup"));
        assert_eq!(outcome.status, 200);
        assert!(
            !setup
                .manager
                .health_report()
                .iter()
                .find(|b| b.id == "primary")
                .expect("primary")
                .healthy,
            "a failed forward must mark the backend down"
        );
    }

    #[tokio::test]
    async fn no_healthy_backends_returns_a_502_json_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![],
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());
        let client = reqwest::Client::new();
        let mut outcome = forward_with_retry(
            &client,
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(outcome.status, 502);
        assert_eq!(outcome.backend_id, None);
        assert_eq!(outcome.endpoint, None);
        let text = body_text(&mut outcome).await;
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(parsed["error"]["message"], "no healthy backends available");
        assert_eq!(parsed["error"]["type"], "server_error");
        assert_eq!(parsed["error"]["code"], 502);
    }

    #[tokio::test]
    async fn backend_api_key_is_sent_as_bearer_to_the_backend() {
        let seen_auth = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let server = {
            let seen_auth = seen_auth.clone();
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap| {
                    let seen_auth = seen_auth.clone();
                    async move {
                        *seen_auth.lock().expect("lock") = headers
                            .get(AUTHORIZATION)
                            .map(|v| v.to_str().unwrap_or("").to_string());
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            );
            testing::spawn_test_server(app).await
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![BackendConfig {
                api_key: Some("sekret".to_string()),
                ..backend("b", server.port)
            }],
            sticky: false,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());
        let outcome = forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(outcome.status, 200);
        assert_eq!(
            seen_auth.lock().expect("lock").as_deref(),
            Some("Bearer sekret")
        );
    }

    #[tokio::test]
    async fn client_auth_key_is_stripped_upstream_when_router_api_keys_are_set() {
        let seen_auth = std::sync::Arc::new(std::sync::Mutex::new(Some("unset".to_string())));
        let server = {
            let seen_auth = seen_auth.clone();
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap| {
                    let seen_auth = seen_auth.clone();
                    async move {
                        *seen_auth.lock().expect("lock") = headers
                            .get(AUTHORIZATION)
                            .map(|v| v.to_str().unwrap_or("").to_string());
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            );
            testing::spawn_test_server(app).await
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![backend("b", server.port)],
            sticky: false,
            api_keys: Some(vec!["router-secret".to_string()]),
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());
        let headers = header_map(&[
            ("content-type", "application/json"),
            ("authorization", "Bearer router-secret"),
        ]);
        let outcome = forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &headers,
                body: &chat_body(),
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(outcome.status, 200);
        assert_eq!(seen_auth.lock().expect("lock").as_deref(), None);
    }

    #[tokio::test]
    async fn client_authorization_is_forwarded_when_the_router_has_no_keys() {
        let seen_auth = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let server = {
            let seen_auth = seen_auth.clone();
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap| {
                    let seen_auth = seen_auth.clone();
                    async move {
                        *seen_auth.lock().expect("lock") = headers
                            .get(AUTHORIZATION)
                            .map(|v| v.to_str().unwrap_or("").to_string());
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            );
            testing::spawn_test_server(app).await
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![backend("b", server.port)],
            sticky: false,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());
        let headers = header_map(&[
            ("content-type", "application/json"),
            ("authorization", "Bearer client-secret"),
        ]);
        forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &headers,
                body: &chat_body(),
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(
            seen_auth.lock().expect("lock").as_deref(),
            Some("Bearer client-secret")
        );
    }

    #[tokio::test]
    async fn host_and_hop_by_hop_headers_are_rewritten() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(HeaderMap::new()));
        let server = {
            let seen = seen.clone();
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().expect("lock") = headers;
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            );
            testing::spawn_test_server(app).await
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![backend("b", server.port)],
            sticky: false,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());
        let headers = header_map(&[
            ("content-type", "application/json"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("content-length", "999999"),
            ("x-keep", "yes"),
        ]);
        let body = chat_body();
        let outcome = forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &headers,
                body: &body,
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(outcome.status, 200);
        let seen = seen.lock().expect("lock");
        assert_eq!(
            seen.get(HOST).map(|v| v.to_str().unwrap_or("")),
            Some(format!("127.0.0.1:{}", server.port).as_str()),
        );
        // Hop-by-hop request headers are dropped, including the stale
        // content-length (the client recomputes it from the body).
        assert!(seen.get("transfer-encoding").is_none());
        assert!(seen.get("connection").is_none());
        let expected_length = serde_json::to_vec(&body).expect("json").len().to_string();
        assert_eq!(
            seen.get("content-length").map(|v| v.to_str().unwrap_or("")),
            Some(expected_length.as_str()),
        );
        // Ordinary headers pass through.
        assert_eq!(seen.get("x-keep").unwrap(), "yes");
        assert_eq!(seen.get("content-type").unwrap(), "application/json");
    }

    #[tokio::test]
    async fn sse_responses_stream_through() {
        let chunks = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let server = {
            let chunks = chunks.clone();
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let chunks = chunks.clone();
                    async move {
                        let body = chunks.join("");
                        (
                            [
                                ("content-type", "text/event-stream"),
                                ("cache-control", "no-cache"),
                            ],
                            body,
                        )
                    }
                }),
            );
            testing::spawn_test_server(app).await
        };

        let setup = {
            let dir = tempfile::tempdir().expect("temp dir");
            let config = Config {
                backends: vec![backend("b", server.port)],
                sticky: false,
                state_dir: Some(dir.path().display().to_string()),
                ..Config::default()
            };
            Setup {
                client: reqwest::Client::new(),
                manager: BackendManager::new(config.clone()),
                config,
                _dir: dir,
            }
        };

        let mut outcome = forward(&setup, &chat_body(), None).await;
        assert_eq!(outcome.status, 200);
        assert_eq!(
            outcome.headers.get("content-type").unwrap(),
            "text/event-stream"
        );
        let text = body_text(&mut outcome).await;
        for chunk in &chunks {
            assert!(text.contains(chunk.as_str()), "missing chunk {chunk:?}");
        }
    }

    #[tokio::test]
    async fn response_status_and_headers_are_relayed() {
        let server = testing::spawn_test_server(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    [
                        ("content-type", "application/json"),
                        ("x-backend", "under-test"),
                        ("connection", "keep-alive"),
                    ],
                    axum::Json(serde_json::json!({"ok": true})),
                )
            }),
        ))
        .await;

        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![backend("b", server.port)],
            sticky: false,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        let manager = BackendManager::new(config.clone());
        let outcome = forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(outcome.status, 200);
        assert_eq!(outcome.headers.get("x-backend").unwrap(), "under-test");
        assert!(outcome.headers.get("connection").is_none());
    }

    /// A backend that records the session headers it was sent.
    async fn session_capture_server() -> (
        testing::TestServer,
        std::sync::Arc<std::sync::Mutex<Option<String>>>,
        std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ) {
        let session = std::sync::Arc::new(std::sync::Mutex::new(None));
        let opencode = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (session_for_handler, opencode_for_handler) = (session.clone(), opencode.clone());
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap| {
                let (session, opencode) =
                    (session_for_handler.clone(), opencode_for_handler.clone());
                async move {
                    *session.lock().expect("lock") = headers
                        .get("x-session-id")
                        .map(|v| v.to_str().unwrap_or("").to_string());
                    *opencode.lock().expect("lock") = headers
                        .get("x-opencode-session")
                        .map(|v| v.to_str().unwrap_or("").to_string());
                    axum::Json(serde_json::json!({"ok": true}))
                }
            }),
        );
        (testing::spawn_test_server(app).await, session, opencode)
    }

    fn single_backend_setup(port: u16) -> (Config, BackendManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = Config {
            backends: vec![backend("primary", port)],
            sticky: false,
            state_dir: Some(dir.path().display().to_string()),
            ..Config::default()
        };
        (config.clone(), BackendManager::new(config), dir)
    }

    #[tokio::test]
    async fn fingerprint_session_key_is_forwarded_as_x_session_id_upstream() {
        let (server, session, opencode) = session_capture_server().await;
        let (config, manager, _dir) = single_backend_setup(server.port);

        forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: Some("f:abc123"),
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(session.lock().expect("lock").as_deref(), Some("abc123"));
        assert_eq!(opencode.lock().expect("lock").as_deref(), None);
    }

    #[tokio::test]
    async fn non_opencode_backends_get_x_session_id_but_not_x_opencode_session() {
        let (server, session, opencode) = session_capture_server().await;
        let (config, manager, _dir) = single_backend_setup(server.port);

        forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: Some("h:my-session"),
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(session.lock().expect("lock").as_deref(), Some("my-session"));
        assert_eq!(opencode.lock().expect("lock").as_deref(), None);
    }

    #[tokio::test]
    async fn client_provided_session_headers_are_preserved_upstream() {
        let (server, session, opencode) = session_capture_server().await;
        let (config, manager, _dir) = single_backend_setup(server.port);

        let headers = header_map(&[
            ("content-type", "application/json"),
            ("x-session-id", "client-val"),
            ("x-opencode-session", "client-opencode"),
        ]);
        forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &headers,
                body: &chat_body(),
                session_key: Some("f:other"),
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(session.lock().expect("lock").as_deref(), Some("client-val"));
        assert_eq!(
            opencode.lock().expect("lock").as_deref(),
            Some("client-opencode")
        );
    }

    #[tokio::test]
    async fn no_session_key_means_no_session_headers_are_injected() {
        let (server, session, opencode) = session_capture_server().await;
        let (config, manager, _dir) = single_backend_setup(server.port);

        forward_with_retry(
            &reqwest::Client::new(),
            ForwardRequest {
                method: Method::POST,
                path: "/v1/chat/completions",
                query: None,
                headers: &chat_headers(),
                body: &chat_body(),
                session_key: None,
            },
            &manager,
            &config,
        )
        .await;

        assert_eq!(session.lock().expect("lock").as_deref(), None);
        assert_eq!(opencode.lock().expect("lock").as_deref(), None);
    }

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("header name");
            map.insert(name, header_value(value));
        }
        map
    }

    #[test]
    fn limit_regex_matches_rate_limit_wordings_only() {
        for message in [
            "rate limit exceeded",
            "Rate Limit",
            "rate_limit",
            "limit reached",
            "you have reached your limit",
            "Too Many Requests",
            "overloaded",
        ] {
            assert!(
                LIMIT_RE.is_match(message),
                "expected a limit match for {message:?}"
            );
            assert!(
                !FUNDS_RE.is_match(message),
                "did not expect a funds match for {message:?}"
            );
        }
        for message in ["ok", "internal error", "model not found", ""] {
            assert!(
                !LIMIT_RE.is_match(message),
                "unexpected limit match for {message:?}"
            );
            assert!(
                !FUNDS_RE.is_match(message),
                "unexpected funds match for {message:?}"
            );
        }
    }

    #[test]
    fn funds_regex_matches_credit_and_quota_wordings() {
        for message in [
            "insufficient_quota",
            "quota exceeded",
            "Insufficient balance",
            "Insufficient funds",
            "CreditsError",
            "payment required",
            "out of credits",
            "please check your plan and billing details",
        ] {
            assert!(
                FUNDS_RE.is_match(message),
                "expected a funds match for {message:?}"
            );
        }
    }

    #[test]
    fn retry_delays_double_from_the_base_and_cap_at_sixty_seconds() {
        assert_eq!(retry_delay_ms(5_000, 0), 5_000);
        assert_eq!(retry_delay_ms(5_000, 1), 10_000);
        assert_eq!(retry_delay_ms(5_000, 2), 20_000);
        assert_eq!(retry_delay_ms(5_000, 3), 40_000);
        assert_eq!(retry_delay_ms(5_000, 4), 60_000);
        assert_eq!(retry_delay_ms(5_000, 9), 60_000);
        assert_eq!(retry_delay_ms(0, 4), 0);
    }
}
