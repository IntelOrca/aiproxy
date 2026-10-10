//! A sandbox backend for aiproxy: a minimal OpenAI-compatible fake server used
//! for testing without real backends.
//!
//! - Validates the requested model against an allowed list and returns a 404
//!   error for unknown models (matching real OpenAI API behavior).
//! - Exposes the allowed models via `GET /models`.
//! - Responds to streaming (SSE) and non-streaming chat completions.
//! - Logs every request to the console and a per-port log file.

use crate::app::Running;
use crate::log::warn;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Models advertised by the sandbox backends.
pub const DEFAULT_MODELS: [&str; 3] = [
    "sandbox-gpt-4o",
    "sandbox-claude-sonnet",
    "sandbox-llama-3.1-70b",
];

/// Options for [`start_sandbox_server`].
pub struct SandboxOptions {
    pub port: u16,
    pub models: Vec<String>,
    pub log_file: Option<String>,
}

impl SandboxOptions {
    pub fn new(port: u16, models: Vec<String>) -> Self {
        Self {
            port,
            models,
            log_file: None,
        }
    }
}

#[derive(Clone)]
struct SandboxState {
    models: Vec<String>,
    log_file: Option<String>,
}

/// Start one fake backend on `127.0.0.1:<port>`.
pub async fn start_sandbox_server(
    port: u16,
    models: Vec<String>,
    log_file: Option<String>,
) -> Running {
    let state = Arc::new(SandboxState { models, log_file });
    let app = Router::new().fallback(handle_request).with_state(state);

    start_backend_on(app, port).await
}

/// Run a single fake backend using the `PORT` and `MODELS` env vars, blocking
/// until shutdown.
pub async fn run_standalone() {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|port| port.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
        .unwrap_or(6060);
    let models: Vec<String> = std::env::var("MODELS")
        .ok()
        .map(|models| {
            models
                .split(',')
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty())
                .collect()
        })
        .filter(|models: &Vec<String>| !models.is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS.iter().map(|m| m.to_string()).collect());

    let state = Arc::new(SandboxState {
        models,
        log_file: Some(format!("aiproxy-sandbox-{port}.log")),
    });
    let app = Router::new().fallback(handle_request).with_state(state);

    println!("[sandbox:{port}] listening on http://127.0.0.1:{port}");
    let running = start_backend_on(app, port).await;
    crate::app::first_shutdown_signal().await;
    running.stop(Duration::from_secs(10)).await;
    std::process::exit(0);
}

/// Bind `app` to `127.0.0.1:<port>` and serve until shutdown.
async fn start_backend_on(app: Router, port: u16) -> Running {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(err) => {
            crate::log::error(format!(
                "could not listen on 127.0.0.1:{port} for a sandbox backend: {err}"
            ));
            std::process::exit(1);
        }
    };
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        server.await.ok();
    });
    Running::new(addr, server, shutdown_tx)
}

fn error(status: StatusCode, message: &str) -> Response {
    let body = json!({
        "error": {"message": message, "type": "invalid_request_error", "code": status.as_u16()}
    });
    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        if let Ok(value) = header::HeaderValue::from_str("application/json") {
            headers.insert(header::CONTENT_TYPE, value);
        }
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).expect("json")))
        .expect("response")
}

fn json_response(value: &Value) -> Response {
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(headers) = builder.headers_mut() {
        if let Ok(value) = header::HeaderValue::from_str("application/json") {
            headers.insert(header::CONTENT_TYPE, value);
        }
    }
    builder
        .body(Body::from(serde_json::to_vec(value).expect("json")))
        .expect("response")
}

fn models_list(models: &[String]) -> Value {
    json!({
        "object": "list",
        "data": models.iter().map(|id| json!({
            "id": id, "object": "model", "owned_by": "sandbox"
        })).collect::<Vec<_>>(),
    })
}

/// The SSE body the fake backend returns for streaming requests.
fn sse_body(model: &str) -> String {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let chunk = json!({
        "id": "chatcmpl-sandbox",
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": "sandbox reply"},
            "finish_reason": null
        }]
    });
    let done = json!({
        "id": "chatcmpl-sandbox",
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&chunk).expect("json"),
        serde_json::to_string(&done).expect("json")
    )
}

async fn handle_request(
    State(state): State<Arc<SandboxState>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    _headers: HeaderMap,
    body: Body,
) -> Response {
    let path = uri.path().to_string();
    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let size_kb = body_bytes.len() as f64 / 1024.0;
    let request_line = format!(
        "[{}] {} {}{} ({:.1} KB)",
        crate::isotime::to_iso8601(crate::isotime::now_ms()),
        method,
        path,
        uri.query().map(|q| format!("?{q}")).unwrap_or_default(),
        size_kb
    );
    println!("[sandbox] {request_line}");
    log_request(&state, &request_line);

    if method == axum::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("response");
    }

    if method == axum::http::Method::GET && path.ends_with("/models") {
        return json_response(&models_list(&state.models));
    }

    if !path.ends_with("/chat/completions") {
        return error(StatusCode::NOT_FOUND, "not found");
    }

    let parsed: Option<Value> = if body_bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(value) => Some(value),
            Err(_) => return error(StatusCode::BAD_REQUEST, "invalid JSON body"),
        }
    };

    let model = parsed
        .as_ref()
        .and_then(|body| body.get("model"))
        .and_then(Value::as_str);
    let Some(model) = model.filter(|model| !model.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "missing model");
    };
    if !state.models.iter().any(|allowed| allowed == model) {
        return error(
            StatusCode::NOT_FOUND,
            &format!("The model '{model}' does not exist or you do not have access to it."),
        );
    }

    let streaming = parsed
        .as_ref()
        .and_then(|body| body.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !streaming {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        return json_response(&json!({
            "id": "chatcmpl-sandbox",
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "sandbox reply"},
                "finish_reason": "stop"
            }]
        }));
    }

    let sse = sse_body(model);
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(headers) = builder.headers_mut() {
        let _ = headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        let _ = headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
        let _ = headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("keep-alive"),
        );
    }
    builder.body(Body::from(sse)).expect("response")
}

/// Append a line to the per-port log file; never let logging break the server.
fn log_request(state: &SandboxState, line: &str) {
    let Some(path) = state.log_file.as_ref() else {
        return;
    };
    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(err) = result {
        warn(format!("sandbox log write failed: {err}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use std::time::Duration;

    async fn start(models: Vec<String>, log_file: Option<String>) -> Running {
        let app = Router::new()
            .fallback(handle_request)
            .with_state(Arc::new(SandboxState { models, log_file }));
        // Bind directly so the test owns the handle (the public helper does
        // the same thing).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });
        Running::new(addr, server, shutdown_tx)
    }

    fn chat_request(model: &str, streaming: Option<bool>) -> Value {
        let mut body = json!({"model": model});
        if let Some(streaming) = streaming {
            body["stream"] = json!(streaming);
        }
        body
    }

    #[tokio::test]
    async fn models_are_listed_on_both_paths() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        for path in ["/v1/models", "/models"] {
            let response = reqwest::get(server_url(&server, path)).await.expect("get");
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = response.json().await.expect("json");
            assert_eq!(body["object"], "list");
            let data = body["data"].as_array().expect("array");
            assert_eq!(data.len(), 3);
            assert_eq!(data[0]["id"], "sandbox-gpt-4o");
            assert_eq!(data[0]["owned_by"], "sandbox");
        }
        server.abort();
    }

    #[tokio::test]
    async fn unknown_models_get_a_404() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        let response = reqwest::Client::new()
            .post(server_url(&server, "/v1/chat/completions"))
            .json(&chat_request("nope", None))
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body: Value = response.json().await.expect("json");
        assert!(body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("does not exist"));
        server.abort();
    }

    #[tokio::test]
    async fn missing_model_gets_a_400() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        let response = reqwest::Client::new()
            .post(server_url(&server, "/v1/chat/completions"))
            .json(&json!({"messages": []}))
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["message"], "missing model");
        server.abort();
    }

    #[tokio::test]
    async fn invalid_json_gets_a_400() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        let response = reqwest::Client::new()
            .post(server_url(&server, "/v1/chat/completions"))
            .body("not json")
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["message"], "invalid JSON body");
        server.abort();
    }

    #[tokio::test]
    async fn non_streaming_requests_get_a_json_completion() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        let response = reqwest::Client::new()
            .post(server_url(&server, "/v1/chat/completions"))
            .json(&chat_request("sandbox-gpt-4o", Some(false)))
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["model"], "sandbox-gpt-4o");
        assert_eq!(body["choices"][0]["message"]["content"], "sandbox reply");
        server.abort();
    }

    #[tokio::test]
    async fn streaming_requests_get_sse() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        let response = reqwest::Client::new()
            .post(server_url(&server, "/v1/chat/completions"))
            .json(&chat_request("sandbox-gpt-4o", Some(true)))
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream"),
        );
        let text = response.text().await.expect("text");
        assert!(text.contains("chat.completion.chunk"));
        assert!(text.contains("sandbox reply"));
        assert!(text.trim_end().ends_with("data: [DONE]"));
        server.abort();
    }

    #[tokio::test]
    async fn requests_default_to_streaming() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;
        let response = reqwest::Client::new()
            .post(server_url(&server, "/v1/chat/completions"))
            .json(&chat_request("sandbox-gpt-4o", None))
            .send()
            .await
            .expect("post");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream"),
        );
        server.abort();
    }

    #[tokio::test]
    async fn unknown_paths_and_options_behave_like_the_deno_server() {
        let server = start(DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(), None).await;

        let response = reqwest::get(server_url(&server, "/nope"))
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["error"]["message"], "not found");

        let response = reqwest::Client::new()
            .request(Method::OPTIONS, server_url(&server, "/v1/chat/completions"))
            .send()
            .await
            .expect("options");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        server.abort();
    }

    #[tokio::test]
    async fn requests_are_logged_to_the_configured_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log_file = dir.path().join("sandbox.log");
        let server = start(
            DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(),
            Some(log_file.display().to_string()),
        )
        .await;
        let response = reqwest::get(server_url(&server, "/v1/models"))
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::OK);
        // Give the append a moment, then check the file.
        let mut logged = false;
        for _ in 0..20 {
            if let Ok(text) = std::fs::read_to_string(&log_file) {
                if text.contains("GET /v1/models") {
                    logged = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(logged, "expected the request to be logged");
        server.abort();
    }

    #[tokio::test]
    async fn standalone_models_env_var_is_parsed() {
        // Mirrors the MODELS env parsing in run_standalone.
        let models = "a, b ,,c"
            .split(',')
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            models,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[tokio::test]
    async fn sse_body_chunks_are_well_formed() {
        let body = sse_body("sandbox-gpt-4o");
        assert!(body.starts_with("data: {"));
        assert!(body.contains("\"object\":\"chat.completion.chunk\""));
        assert!(body.contains("\"content\":\"sandbox reply\""));
        assert!(body.trim_end().ends_with("data: [DONE]"));
        let parsed: Value = serde_json::from_str(
            body.trim_start_matches("data: ")
                .split("\n\n")
                .next()
                .expect("chunk"),
        )
        .expect("json");
        assert_eq!(parsed["choices"][0]["delta"]["content"], "sandbox reply");
    }

    fn server_url(server: &Running, path: &str) -> String {
        format!("http://{}{}", server.addr(), path)
    }

    #[tokio::test]
    async fn the_public_helper_starts_a_listening_server() {
        let server = start_sandbox_server(
            0,
            DEFAULT_MODELS.iter().map(|m| m.to_string()).collect(),
            None,
        )
        .await;
        assert_ne!(server.addr().port(), 0);
        let response = reqwest::get(server_url(&server, "/v1/models"))
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::OK);
        server.abort();
    }
}
