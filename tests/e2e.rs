//! End-to-end tests: the real router over HTTP, with fake backends.

use aiproxy::app::{run_until_shutdown, start_aiproxy, Running};
use aiproxy::testing::{spawn_router, spawn_test_server};
use aiproxy::types::{BackendConfig, Config};
use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MODELS: [&str; 3] = [
    "sandbox-gpt-4o",
    "sandbox-claude-sonnet",
    "sandbox-llama-3.1-70b",
];

/// A fake OpenAI-compatible backend.
fn fake_backend(models: &[&'static str], status: StatusCode) -> Router {
    let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();
    Router::new()
        .route(
            "/v1/models",
            get(move || {
                let models = models.clone();
                async move {
                    axum::Json(json!({
                        "object": "list",
                        "data": models.iter().map(|id| json!({
                            "id": id, "object": "model", "owned_by": "fake"
                        })).collect::<Vec<_>>(),
                    }))
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move || {
                let status = status;
                async move {
                    let body = json!({
                        "id": "chatcmpl-fake",
                        "object": "chat.completion",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": "fake reply"}, "finish_reason": "stop"}]
                    });
                    (status, axum::Json(body))
                }
            }),
        )
}

/// A backend that answers chat completions with an SSE stream.
fn sse_backend() -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
            (
                [
                    (header::CONTENT_TYPE, "text/event-stream"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                body,
            )
        }),
    )
}

/// A backend that records what it was asked for.
struct Recording {
    server: aiproxy::testing::TestServer,
    hits: Arc<AtomicUsize>,
    sessions: Arc<Mutex<Vec<String>>>,
}

impl Recording {
    async fn new() -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let handler_hits = Arc::clone(&hits);
        let handler_sessions = Arc::clone(&sessions);
        let app = Router::new()
            .route(
                "/v1/models",
                get(|| async { axum::Json(json!({"object": "list", "data": []})) }),
            )
            .route(
                "/v1/chat/completions",
                post(move |headers: axum::http::HeaderMap| {
                    let hits = handler_hits.clone();
                    let sessions = handler_sessions.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if let Some(value) = headers.get("x-session-id") {
                            sessions
                                .lock()
                                .expect("lock")
                                .push(value.to_str().unwrap_or_default().to_string());
                        }
                        axum::Json(json!({"ok": true, "backend": "recording"}))
                    }
                }),
            );
        Self {
            server: spawn_test_server(app).await,
            hits,
            sessions,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn sessions(&self) -> Vec<String> {
        self.sessions.lock().expect("lock").clone()
    }
}

/// Holds the running router, its fake backends and the temp state dir so all
/// of them outlive the test body.
#[allow(dead_code)]
struct Cluster {
    running: Running,
    backends: Vec<aiproxy::testing::TestServer>,
    base_url: String,
    state_dir: tempfile::TempDir,
}

/// Start a router wired to `count` fake backends on ephemeral ports.
async fn cluster(count: usize, customize: impl FnOnce(&mut Config)) -> Cluster {
    let mut backends = Vec::new();
    let mut configured = Vec::new();
    for i in 0..count {
        let server = spawn_test_server(fake_backend(&MODELS, StatusCode::OK)).await;
        configured.push(BackendConfig {
            priority: Some(i as i64),
            ..BackendConfig::new(format!("backend-{i}"), format!("{}/v1", server.base_url()))
        });
        backends.push(server);
    }

    let state_dir = tempfile::tempdir().expect("temp dir");
    let mut config = Config {
        port: 0,
        backends: configured,
        sticky: true,
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        max_retries: 1,
        models: Some(MODELS.iter().map(|m| m.to_string()).collect()),
        state_dir: Some(state_dir.path().display().to_string()),
        ..Config::default()
    };
    customize(&mut config);

    let running = start_aiproxy(config).await.expect("start router");
    let base_url = format!("http://{}", running.addr());
    Cluster {
        running,
        backends,
        base_url,
        state_dir,
    }
}

/// Convert a reqwest response into an axum one so the test helpers can share
/// a single response type.
fn into_axum_response(response: reqwest::Response) -> Response {
    let mut builder = Response::builder().status(response.status().as_u16());
    if let Some(headers) = builder.headers_mut() {
        *headers = response.headers().clone();
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .expect("response")
}

impl Cluster {
    async fn get(&self, path: &str) -> Response {
        into_axum_response(
            reqwest::Client::new()
                .get(format!("{}{path}", self.base_url))
                .send()
                .await
                .expect("request"),
        )
    }

    async fn post_json(&self, path: &str, body: &Value, headers: &[(&str, &str)]) -> Response {
        let mut request = reqwest::Client::new().post(format!("{}{path}", self.base_url));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        into_axum_response(request.json(body).send().await.expect("request"))
    }

    async fn post_raw(&self, path: &str, body: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = reqwest::Client::new().post(format!("{}{path}", self.base_url));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        into_axum_response(
            request
                .body(body.to_string())
                .send()
                .await
                .expect("request"),
        )
    }

    async fn get_with_headers(&self, path: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = reqwest::Client::new().get(format!("{}{path}", self.base_url));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        into_axum_response(request.send().await.expect("request"))
    }
}

async fn status_json(cluster: &Cluster) -> Value {
    let response = cluster.get("/api/status").await;
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// --- dashboard and status -----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_is_served_at_root() {
    let cluster = cluster(1, |_| {}).await;
    let response = cluster.get("/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8"),
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("aiproxy dashboard"));
    assert!(html.contains("/api/status"));

    // /index.html is an alias.
    assert_eq!(cluster.get("/index.html").await.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_json_reports_config_backends_and_recent_routes() {
    let cluster = cluster(2, |_| {}).await;

    // Route one chat completion so stats have something to show.
    let response = cluster
        .post_json(
            "/v1/chat/completions",
            &json!({"model": "sandbox-gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let status = status_json(&cluster).await;
    assert_eq!(status["service"], "aiproxy");
    assert_eq!(status["config"]["port"], 0);
    assert_eq!(status["config"]["sticky"], true);
    assert_eq!(status["config"]["recentRoutes"], 200);
    assert_eq!(status["config"]["authRequired"], false);
    assert_eq!(status["config"]["retryStatusCodes"], json!([429, 529]));
    assert_eq!(status["config"]["retryOnLimitMessage"], true);
    assert_eq!(status["config"]["shutdownGraceMs"], 10_000);

    let backends = status["backends"].as_array().expect("backends");
    assert_eq!(backends.len(), 2);
    assert_eq!(backends[0]["id"], "backend-0");
    assert_eq!(backends[0]["healthy"], true);
    assert_eq!(backends[0]["priority"], 0);
    // Priority is omitted from JSON when unset, like `undefined` in JS.
    assert_eq!(backends[0]["requests"].as_u64(), Some(1));
    assert_eq!(
        status["models"],
        json!([
            "sandbox-gpt-4o",
            "sandbox-claude-sonnet",
            "sandbox-llama-3.1-70b"
        ])
    );

    let recent = status["recent"].as_array().expect("recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["backendId"], "backend-0");
    assert_eq!(recent[0]["model"], "sandbox-gpt-4o");
    assert_eq!(recent[0]["status"], 200);
    assert!(recent[0]["sessionId"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert!(recent[0]["at"].as_str().is_some_and(|s| s.ends_with('Z')));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_is_omitted_when_unset() {
    let cluster = cluster(1, |config| {
        config.backends[0].priority = None;
    })
    .await;
    let status = status_json(&cluster).await;
    assert!(status["backends"][0].get("priority").is_none());
}

// --- models -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_models_are_served_from_config() {
    let cluster = cluster(1, |_| {}).await;
    let response = cluster.get("/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().expect("data");
    assert_eq!(data.len(), 3);
    assert_eq!(data[0]["id"], "sandbox-gpt-4o");
    assert_eq!(data[0]["owned_by"], "aiproxy");

    // /models is an alias.
    assert_eq!(cluster.get("/models").await.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_are_proxied_from_a_backend_when_not_configured() {
    let cluster = cluster(1, |config| {
        // No static model list: the router proxies /models from a backend.
        config.models = None;
    })
    .await;

    let response = cluster.get("/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["data"][0]["owned_by"], "fake");
    assert_eq!(body["data"].as_array().expect("data").len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_fall_back_to_the_default_list_without_healthy_backends() {
    let cluster = cluster(1, |config| {
        config.backends[0] = BackendConfig::new("dead", "http://127.0.0.1:1/v1");
        // No static list, so the router falls back to the default.
        config.models = None;
    })
    .await;

    // Wait for the initial health check to mark the dead backend down.
    for _ in 0..50 {
        let status = status_json(&cluster).await;
        if status["backends"][0]["healthy"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = status_json(&cluster).await;
    assert_eq!(status["backends"][0]["healthy"], false);

    let response = cluster.get("/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["data"][0]["id"], "gpt-4o");
}

// --- chat completions ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_are_forwarded_and_the_response_relayed() {
    let cluster = cluster(1, |_| {}).await;
    let response = cluster
        .post_json(
            "/v1/chat/completions",
            &json!({"model": "sandbox-gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["choices"][0]["message"]["content"], "fake reply");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sticky_sessions_keep_hitting_the_same_backend() {
    let cluster = cluster(2, |_| {}).await;
    let body = json!({"model": "sandbox-gpt-4o", "messages": [
        {"role": "system", "content": "you are a bot"},
        {"role": "user", "content": "first turn"}
    ]});

    for _ in 0..4 {
        let response = cluster.post_json("/v1/chat/completions", &body, &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    let status = status_json(&cluster).await;
    let backends = status["backends"].as_array().expect("backends");
    let served: Vec<u64> = backends
        .iter()
        .map(|b| b["requests"].as_u64().expect("requests"))
        .collect();
    // All four turns of one conversation land on a single backend.
    assert_eq!(served.iter().sum::<u64>(), 4);
    assert_eq!(served.iter().filter(|n| **n > 0).count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_responses_stream_through_the_router() {
    let backend = spawn_test_server(sse_backend()).await;
    let state_dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        port: 0,
        backends: vec![BackendConfig::new(
            "sse",
            format!("{}/v1", backend.base_url()),
        )],
        sticky: false,
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        state_dir: Some(state_dir.path().display().to_string()),
        ..Config::default()
    };
    let running = start_aiproxy(config).await.expect("start router");
    let base_url = format!("http://{}", running.addr());

    let response = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&json!({"model": "sandbox-gpt-4o", "messages": [], "stream": true}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
    );
    let text = response.text().await.expect("body");
    assert!(text.contains("data: {\"choices\""), "got: {text}");
    assert!(text.contains("data: [DONE]"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limited_backend_fails_over_and_records_the_serving_backend() {
    let limited = spawn_test_server(
        Router::new()
            .route(
                "/v1/models",
                get(|| async { axum::Json(json!({"object": "list", "data": []})) }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(json!({"error": {"message": "rate limit"}})),
                    )
                }),
            ),
    )
    .await;
    let healthy = spawn_test_server(fake_backend(&MODELS, StatusCode::OK)).await;

    let state_dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        port: 0,
        backends: vec![
            BackendConfig {
                priority: Some(0),
                ..BackendConfig::new("limited", format!("{}/v1", limited.base_url()))
            },
            BackendConfig {
                priority: Some(1),
                ..BackendConfig::new("healthy", format!("{}/v1", healthy.base_url()))
            },
        ],
        sticky: true,
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        max_retries: 1,
        state_dir: Some(state_dir.path().display().to_string()),
        ..Config::default()
    };
    let running = start_aiproxy(config).await.expect("start router");
    let base_url = format!("http://{}", running.addr());

    let body =
        json!({"model": "sandbox-gpt-4o", "messages": [{"role": "user", "content": "turn"}]});
    let response = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);

    let status: Value = reqwest::Client::new()
        .get(format!("{base_url}/api/status"))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    let recent = status["recent"].as_array().expect("recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["backendId"], "healthy");
    // The limited backend stays healthy (a rate limit is not "down").
    let backends = status["backends"].as_array().expect("backends");
    assert_eq!(backends[0]["healthy"], true);
}

// --- errors and auth ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_routes_return_404_json() {
    let cluster = cluster(1, |_| {}).await;
    let response = cluster.get("/nope").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "no route for GET /nope");
    assert_eq!(body["error"]["code"], 404);

    let response = cluster.post_json("/v1/nope", &json!({}), &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error"]["message"], "no route for POST /v1/nope");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_json_bodies_are_rejected() {
    let cluster = cluster(1, |_| {}).await;
    let response = cluster
        .post_raw("/v1/chat/completions", "not json", &[])
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error"]["message"], "invalid JSON body");

    // An empty body is invalid JSON too.
    let response = cluster.post_raw("/v1/chat/completions", "", &[]).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn options_requests_are_answered_with_204() {
    let cluster = cluster(1, |_| {}).await;
    let response = reqwest::Client::new()
        .request(
            Method::OPTIONS,
            format!("{}/v1/chat/completions", cluster.base_url),
        )
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_api_keys_gate_models_and_chat_completions() {
    let cluster = cluster(1, |config| {
        config.api_keys = Some(vec!["router-secret".to_string()]);
    })
    .await;

    // No key: 401 on both endpoints.
    let response = cluster.get("/v1/models").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error"]["message"], "invalid or missing API key");

    let response = cluster
        .post_json(
            "/v1/chat/completions",
            &json!({"model": "sandbox-gpt-4o", "messages": []}),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Wrong key: still 401.
    let response = cluster
        .post_json(
            "/v1/chat/completions",
            &json!({"model": "sandbox-gpt-4o", "messages": []}),
            &[("authorization", "Bearer nope")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Right key: through.
    let response = cluster
        .post_json(
            "/v1/chat/completions",
            &json!({"model": "sandbox-gpt-4o", "messages": []}),
            &[("authorization", "Bearer router-secret")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = cluster
        .get_with_headers("/v1/models", &[("authorization", "bearer router-secret")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    // The dashboard and status stay open.
    assert_eq!(cluster.get("/api/status").await.status(), StatusCode::OK);
    assert_eq!(cluster.get("/").await.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_keys_are_not_forwarded_upstream_but_backend_keys_are() {
    let recording = Recording::new().await;
    let state_dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        port: 0,
        backends: vec![BackendConfig {
            api_key: Some("backend-secret".to_string()),
            ..BackendConfig::new("b", format!("{}/v1", recording.server.base_url()))
        }],
        sticky: false,
        api_keys: Some(vec!["router-secret".to_string()]),
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        state_dir: Some(state_dir.path().display().to_string()),
        ..Config::default()
    };
    let running = start_aiproxy(config).await.expect("start router");
    let base_url = format!("http://{}", running.addr());

    let response = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .header("authorization", "Bearer router-secret")
        .json(&json!({"model": "sandbox-gpt-4o", "messages": []}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(recording.hits(), 1);
}

// --- session headers ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_keys_are_forwarded_upstream() {
    let recording = Recording::new().await;
    let state_dir = tempfile::tempdir().expect("temp dir");
    let config = Config {
        port: 0,
        backends: vec![BackendConfig::new(
            "b",
            format!("{}/v1", recording.server.base_url()),
        )],
        sticky: false,
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        state_dir: Some(state_dir.path().display().to_string()),
        ..Config::default()
    };
    let running = start_aiproxy(config).await.expect("start router");
    let base_url = format!("http://{}", running.addr());

    let body = json!({"model": "sandbox-gpt-4o", "messages": [
        {"role": "user", "content": "hello"}
    ]});
    for _ in 0..2 {
        let response = reqwest::Client::new()
            .post(format!("{base_url}/v1/chat/completions"))
            .json(&body)
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::OK);
    }

    // Both turns fingerprint to the same conversation, so the same
    // conversation ID is sent upstream both times.
    assert_eq!(recording.hits(), 2);
    let sessions = recording.sessions();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0], sessions[1]);
    assert!(sessions[0].starts_with("f:") || sessions[0].len() == 24);
}

// --- persistence --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_survive_a_restart() {
    let state_dir = tempfile::tempdir().expect("temp dir");
    let backend = spawn_test_server(fake_backend(&MODELS, StatusCode::OK)).await;
    let backend_url = format!("{}/v1", backend.base_url());
    let state = state_dir.path().display().to_string();

    let config = || Config {
        port: 0,
        backends: vec![BackendConfig::new("b", backend_url.clone())],
        sticky: true,
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        state_dir: Some(state.clone()),
        ..Config::default()
    };

    let running = start_aiproxy(config()).await.expect("start router");
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", running.addr()))
        .json(&json!({"model": "sandbox-gpt-4o", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    // stop() flushes stats and pins to disk.
    running.stop(Duration::from_millis(500)).await;

    let stats: Value = serde_json::from_str(
        &std::fs::read_to_string(state_dir.path().join("stats.json")).expect("stats file"),
    )
    .expect("json");
    assert_eq!(stats["backendCounts"]["b"], 1);
    assert_eq!(stats["recent"].as_array().expect("array").len(), 1);

    // A fresh router restores the dashboard state.
    let restarted = start_aiproxy(config()).await.expect("restart router");
    let status: Value = reqwest::Client::new()
        .get(format!("http://{}/api/status", restarted.addr()))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(status["backends"][0]["requests"], 1);
    assert_eq!(status["recent"].as_array().expect("array").len(), 1);
    restarted.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pins_survive_a_restart() {
    let state_dir = tempfile::tempdir().expect("temp dir");
    let recording = Recording::new().await;
    let backend_url = format!("{}/v1", recording.server.base_url());
    let state = state_dir.path().display().to_string();

    let config = || Config {
        port: 0,
        backends: vec![BackendConfig::new("b", backend_url.clone())],
        sticky: true,
        health_check_interval_ms: 600_000,
        health_check_timeout_ms: 1_000,
        session_ttl_ms: 60_000,
        upstream_timeout_ms: 5_000,
        state_dir: Some(state.clone()),
        ..Config::default()
    };

    let running = start_aiproxy(config()).await.expect("start router");
    let body = json!({"model": "sandbox-gpt-4o", "messages": [{"role": "user", "content": "remember me"}]});
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", running.addr()))
        .json(&body)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    running.stop(Duration::from_millis(500)).await;

    let pins: Value = serde_json::from_str(
        &std::fs::read_to_string(state_dir.path().join("pins.json")).expect("pins file"),
    )
    .expect("json");
    let pins = pins["pins"].as_object().expect("pins");
    assert_eq!(pins.len(), 1);
    let pin = pins.values().next().expect("pin");
    assert_eq!(pin["backendId"], "b");

    // A restarted router keeps routing that conversation to "b".
    let restarted = start_aiproxy(config()).await.expect("restart router");
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", restarted.addr()))
        .json(&body)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    restarted.abort();
}

// --- shutdown -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_are_rejected_with_503_once_shutting_down() {
    let cluster = cluster(1, |_| {}).await;
    assert!(
        cluster.running.begin_shutdown(),
        "the first call initiates shutdown"
    );
    assert!(
        !cluster.running.begin_shutdown(),
        "a second call reports an in-progress shutdown"
    );

    let response = cluster.get("/api/status").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error"]["message"], "server is shutting down");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_stop_flushes_state() {
    let cluster = cluster(1, |_| {}).await;
    cluster
        .post_json(
            "/v1/chat/completions",
            &json!({"model": "sandbox-gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
            &[],
        )
        .await;
    cluster.running.stop(Duration::from_millis(1000)).await;

    let stats_path = cluster.state_dir.path().join("stats.json");
    let text = std::fs::read_to_string(&stats_path).expect("stats flushed on stop");
    let stats: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(stats["backendCounts"]["backend-0"], 1);
}

/// `run_until_shutdown` exits the process, so it is only exercised by
/// compiling it here; the signal paths are covered by the sandbox smoke run.
#[test]
fn run_until_shutdown_is_available_for_main() {
    let _ = run_until_shutdown;
}

/// `spawn_router` is used by the sandbox server tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_router_helper_binds_an_ephemeral_port() {
    let (addr, shutdown, task) =
        spawn_router(Router::new().route("/", get(|| async { "hello" }))).await;
    let response = reqwest::get(format!("http://{addr}/")).await.expect("get");
    assert_eq!(response.text().await.expect("text"), "hello");
    let _ = shutdown.send(());
    let _ = task.await;
}
