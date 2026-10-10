//! Configuration loading.
//!
//! The Deno version parsed `config.json` into an untyped object and read every
//! field with JavaScript coercion (`Number(x) || default`, `Boolean(x)`,
//! `String(x)`). That tolerance is deliberate — a malformed or partial config
//! must still start the router with defaults — so the coercion rules are
//! reproduced here rather than using `serde`'s strict, all-or-nothing parsing.

use crate::log::warn;
use crate::types::{BackendConfig, Config};
use serde_json::Value;
use std::path::Path;

/// Fallback backend used when no backends are configured.
const DEFAULT_BACKEND_URL: &str = "http://127.0.0.1:3456/v1";

/// Load the router config from `path`, falling back to defaults for anything
/// missing or invalid.
pub fn load_config(path: impl AsRef<Path>) -> Config {
    let path = path.as_ref();
    let partial = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Some(map),
            _ => {
                warn(format!(
                    "{} not found or invalid; using defaults.",
                    path.display()
                ));
                None
            }
        },
        Err(_) => {
            warn(format!(
                "{} not found or invalid; using defaults.",
                path.display()
            ));
            None
        }
    };

    let get = |key: &str| -> Option<&Value> { partial.as_ref().and_then(|m| m.get(key)) };

    let mut backends = load_backends(get("backends"));
    let retry_status_codes = match get("retryStatusCodes") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|v| {
                    js_to_number(v).filter(|n| {
                        n.is_finite() && *n >= 0.0 && *n <= f64::from(u16::MAX) && n.fract() == 0.0
                    })
                })
                .map(|n| n as u16)
                .collect(),
        ),
        _ => None,
    };

    if backends.is_empty() {
        warn(
            "no backends configured; defaulting to http://127.0.0.1:3456/v1 (opencode default port).",
        );
        backends.push(BackendConfig::new("backend-1", DEFAULT_BACKEND_URL));
    }

    Config {
        port: load_port(get("port")),
        backends,
        sticky: bool_or(get("sticky"), true),
        health_check_interval_ms: number_or(get("healthCheckIntervalMs"), 10_000.0) as u64,
        health_check_timeout_ms: number_or(get("healthCheckTimeoutMs"), 2_500.0) as u64,
        session_ttl_ms: number_or(get("sessionTtlMs"), 30.0 * 60_000.0) as u64,
        upstream_timeout_ms: number_or(get("upstreamTimeoutMs"), 600_000.0) as u64,
        max_retries: number_or(get("maxRetries"), 1.0) as u64,
        models: match get("models") {
            Some(Value::Array(items)) => Some(items.iter().map(js_to_string).collect()),
            _ => None,
        },
        // Only string values are kept; the Deno version passed other JSON
        // types through, which would rewrite the model to a non-string.
        model_map: match get("modelMap") {
            Some(Value::Object(map)) => Some(
                map.iter()
                    .filter_map(|(k, v)| match v {
                        Value::String(s) => Some((k.clone(), s.clone())),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        },
        api_keys: match get("apiKeys") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(js_to_string)
                    .filter(|k| !k.is_empty())
                    .collect(),
            ),
            _ => None,
        },
        retry_status_codes,
        retry_on_limit_message: bool_or(get("retryOnLimitMessage"), true),
        state_dir: match get("stateDir") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        },
        recent_routes: number_or(get("recentRoutes"), 200.0) as u64,
        shutdown_grace_ms: number_or(get("shutdownGraceMs"), 10_000.0) as u64,
    }
}

fn load_backends(raw: Option<&Value>) -> Vec<BackendConfig> {
    let Some(Value::Array(raw_backends)) = raw else {
        return Vec::new();
    };
    raw_backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let field = |key: &str| -> Option<&Value> { b.as_object().and_then(|m| m.get(key)) };
            BackendConfig {
                id: match field("id") {
                    Some(Value::String(s)) if !s.is_empty() => s.clone(),
                    _ => format!("backend-{}", i + 1),
                },
                base_url: match field("baseUrl") {
                    None | Some(Value::Null) => DEFAULT_BACKEND_URL.to_string(),
                    Some(v) => js_to_string(v),
                }
                .trim_end_matches('/')
                .to_string(),
                weight: number_or(field("weight"), 1.0),
                model: match field("model") {
                    Some(Value::String(s)) => Some(s.clone()),
                    _ => None,
                },
                api_key: match field("apiKey") {
                    Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                },
                priority: field("priority").and_then(js_to_number).map(|n| n as i64),
            }
        })
        .collect()
}

/// Load the listen port. Mirrors `Number(port) || 8080`, with the extra
/// constraint that the result must be a valid TCP port.
fn load_port(raw: Option<&Value>) -> u16 {
    let default = 8080;
    match raw {
        Some(v) if !v.is_null() => match js_to_number(v) {
            Some(n) if n >= 1.0 && n <= f64::from(u16::MAX) && n.fract() == 0.0 => n as u16,
            // Zero and unusable values silently fall back, matching the Deno
            // `0 || default` behavior.
            other => {
                if other.is_some_and(|n| n != 0.0) {
                    warn(format!("invalid port {}; using {}.", display(raw), default));
                }
                default
            }
        },
        _ => default,
    }
}

fn display(v: Option<&Value>) -> String {
    v.map(js_to_string).unwrap_or_default()
}

// --- JavaScript coercion helpers ------------------------------------------------

/// TS `Number(value ?? default) || default`: zero, non-numeric and non-finite
/// values all fall back to `default`.
fn number_or(value: Option<&Value>, default: f64) -> f64 {
    match value {
        Some(v) if !v.is_null() => js_to_number(v)
            .filter(|n| n.is_finite() && *n != 0.0)
            .unwrap_or(default),
        _ => default,
    }
}

/// TS `Boolean(value ?? default)`.
fn bool_or(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(v) if !v.is_null() => match js_to_number(v) {
            Some(n) => n != 0.0,
            None => match v {
                Value::String(s) => !s.is_empty(),
                _ => true, // arrays and objects are truthy
            },
        },
        _ => default,
    }
}

/// TS `Number()` for JSON scalars; `None` means the result would be NaN.
fn js_to_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        Value::String(s) => js_number_from_string(s),
        // Objects/arrays coerce to 0 or NaN depending on shape; treat both as
        // "not a number" so the caller applies its default.
        _ => None,
    }
}

/// TS `Number(string)` for the cases a config file can sensibly contain.
fn js_number_from_string(s: &str) -> Option<f64> {
    let trimmed = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return Some(0.0);
    }
    if trimmed.eq_ignore_ascii_case("nan")
        || trimmed.eq_ignore_ascii_case("infinity")
        || trimmed.eq_ignore_ascii_case("+infinity")
        || trimmed.eq_ignore_ascii_case("-infinity")
    {
        return None;
    }
    trimmed.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// TS `String()` for JSON values.
pub fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|it| {
                if it.is_null() {
                    String::new()
                } else {
                    js_to_string(it)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn write_config(dir: &tempfile::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write config");
        path
    }

    #[test]
    fn defaults_when_file_missing() {
        let dir = temp_dir();
        let config = load_config(dir.path().join("nope.json"));
        assert_eq!(config.port, 8080);
        assert_eq!(config.health_check_interval_ms, 10_000);
        assert_eq!(config.health_check_timeout_ms, 2_500);
        assert_eq!(config.session_ttl_ms, 30 * 60_000);
        assert_eq!(config.upstream_timeout_ms, 600_000);
        assert_eq!(config.max_retries, 1);
        assert_eq!(config.recent_routes, 200);
        assert_eq!(config.shutdown_grace_ms, 10_000);
        assert!(config.sticky);
        assert!(config.retry_on_limit_message);
        assert_eq!(config.retry_codes(), vec![429, 529]);
        assert!(!config.auth_required());
    }

    #[test]
    fn defaults_when_file_invalid() {
        let dir = temp_dir();
        let path = write_config(&dir, "bad.json", "not json");
        let config = load_config(&path);
        assert_eq!(config.port, 8080);
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].id, "backend-1");
        assert_eq!(config.backends[0].base_url, "http://127.0.0.1:3456/v1");
    }

    #[test]
    fn defaults_when_json_is_not_an_object() {
        let dir = temp_dir();
        let path = write_config(&dir, "array.json", "[1, 2, 3]");
        let config = load_config(&path);
        assert_eq!(config.port, 8080);
        assert_eq!(config.backends.len(), 1);
    }

    #[test]
    fn zero_valued_numbers_fall_back_to_defaults() {
        // Deno: `Number(0) || default` — an explicit 0 always became the default.
        let dir = temp_dir();
        let path = write_config(
            &dir,
            "zeros.json",
            r#"{"port":0,"healthCheckIntervalMs":0,"maxRetries":0,"recentRoutes":0,"weight":0}"#,
        );
        let config = load_config(&path);
        assert_eq!(config.port, 8080);
        assert_eq!(config.health_check_interval_ms, 10_000);
        assert_eq!(config.max_retries, 1);
        assert_eq!(config.recent_routes, 200);
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].weight, 1.0);
    }

    #[test]
    fn non_numeric_numbers_fall_back_to_defaults() {
        let dir = temp_dir();
        let path = write_config(&dir, "nan.json", r#"{"port":"abc","maxRetries":"7"}"#);
        let config = load_config(&path);
        // `Number("abc")` is NaN -> default, `Number("7")` is 7.
        assert_eq!(config.port, 8080);
        assert_eq!(config.max_retries, 7);
    }

    #[test]
    fn fractional_numbers_are_truncated() {
        let dir = temp_dir();
        let path = write_config(&dir, "frac.json", r#"{"sessionTtlMs":1500.9}"#);
        let config = load_config(&path);
        assert_eq!(config.session_ttl_ms, 1500);
    }

    #[test]
    fn out_of_range_port_falls_back() {
        let dir = temp_dir();
        let path = write_config(&dir, "port.json", r#"{"port":70000}"#);
        let config = load_config(&path);
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn explicit_port_is_used() {
        let dir = temp_dir();
        let path = write_config(&dir, "port.json", r#"{"port":9999}"#);
        assert_eq!(load_config(&path).port, 9999);
    }

    #[test]
    fn booleans_follow_javascript_truthiness() {
        let dir = temp_dir();
        let path = write_config(
            &dir,
            "bool.json",
            r#"{"sticky":false,"retryOnLimitMessage":"false"}"#,
        );
        let config = load_config(&path);
        assert!(!config.sticky);
        // `Boolean("false")` is true — non-empty strings are truthy.
        assert!(config.retry_on_limit_message);
    }

    #[test]
    fn recent_routes_defaults_to_200_and_honors_explicit_value() {
        let dir = temp_dir();
        let plain = write_config(&dir, "plain.json", r#"{"backends":[]}"#);
        assert_eq!(load_config(&plain).recent_routes, 200);

        let custom = write_config(&dir, "custom.json", r#"{"recentRoutes":10,"backends":[]}"#);
        assert_eq!(load_config(&custom).recent_routes, 10);
    }

    #[test]
    fn backends_are_parsed_tolerantly() {
        let dir = temp_dir();
        let path = write_config(
            &dir,
            "backends.json",
            r#"{"backends":[
                {"id":"b1","baseUrl":"http://127.0.0.1:3456/v1///","weight":3,"priority":2,"model":"m","apiKey":"k"},
                {"id":"","baseUrl":123},
                "not an object",
                {"id":"b4"}
            ]}"#,
        );
        let config = load_config(&path);
        let backends = &config.backends;
        assert_eq!(backends.len(), 4);

        assert_eq!(backends[0].id, "b1");
        assert_eq!(backends[0].base_url, "http://127.0.0.1:3456/v1");
        assert_eq!(backends[0].weight, 3.0);
        assert_eq!(backends[0].priority, Some(2));
        assert_eq!(backends[0].model.as_deref(), Some("m"));
        assert_eq!(backends[0].api_key.as_deref(), Some("k"));

        // Empty id falls back to backend-<index+1>; non-strings stringify.
        assert_eq!(backends[1].id, "backend-2");
        assert_eq!(backends[1].base_url, "123");
        assert_eq!(backends[1].weight, 1.0);
        assert_eq!(backends[1].model, None);
        assert_eq!(backends[1].api_key, None);

        // Non-object entries fall back entirely.
        assert_eq!(backends[2].id, "backend-3");
        assert_eq!(backends[2].base_url, "http://127.0.0.1:3456/v1");

        // Missing fields keep their defaults.
        assert_eq!(backends[3].id, "b4");
        assert_eq!(backends[3].priority, None);
    }

    #[test]
    fn no_backends_configured_falls_back_to_opencode_default() {
        let dir = temp_dir();
        let path = write_config(&dir, "empty.json", r#"{"backends":[]}"#);
        let config = load_config(&path);
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].id, "backend-1");
        assert_eq!(config.backends[0].base_url, "http://127.0.0.1:3456/v1");
        assert_eq!(config.backends[0].weight, 1.0);
    }

    #[test]
    fn models_are_stringified_and_kept() {
        let dir = temp_dir();
        let path = write_config(&dir, "models.json", r#"{"models":["gpt-4o",5,true]}"#);
        let config = load_config(&path);
        assert_eq!(
            config.models,
            Some(vec![
                "gpt-4o".to_string(),
                "5".to_string(),
                "true".to_string()
            ])
        );
    }

    #[test]
    fn model_map_keeps_string_values_only() {
        let dir = temp_dir();
        let path = write_config(
            &dir,
            "map.json",
            r#"{"modelMap":{"gpt-4o":"claude","x":7}}"#,
        );
        let config = load_config(&path);
        let map = config.model_map.expect("model map");
        assert_eq!(map.get("gpt-4o").map(String::as_str), Some("claude"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn api_keys_filter_empty_entries() {
        let dir = temp_dir();
        let path = write_config(&dir, "keys.json", r#"{"apiKeys":["k1","",5,null]}"#);
        let config = load_config(&path);
        assert_eq!(
            config.api_keys,
            Some(vec!["k1".to_string(), "5".to_string(), "null".to_string()])
        );
        assert!(config.auth_required());
    }

    #[test]
    fn empty_api_keys_array_disables_auth() {
        let dir = temp_dir();
        let path = write_config(&dir, "keys.json", r#"{"apiKeys":[]}"#);
        assert!(!load_config(&path).auth_required());
    }

    #[test]
    fn retry_status_codes_default_when_absent_and_keep_when_empty() {
        let dir = temp_dir();
        let absent = write_config(&dir, "a.json", r#"{}"#);
        assert_eq!(load_config(&absent).retry_codes(), vec![429, 529]);

        // An explicit empty array stays empty (no codes trigger failover).
        let empty = write_config(&dir, "b.json", r#"{"retryStatusCodes":[]}"#);
        assert_eq!(load_config(&empty).retry_codes(), Vec::<u16>::new());

        // Non-numeric entries are dropped.
        let filtered = write_config(&dir, "c.json", r#"{"retryStatusCodes":[418,"x",99999]}"#);
        assert_eq!(load_config(&filtered).retry_codes(), vec![418]);
    }

    #[test]
    fn state_dir_requires_a_non_empty_string() {
        let dir = temp_dir();
        let path = write_config(&dir, "state.json", r#"{"stateDir":"/tmp/state"}"#);
        assert_eq!(load_config(&path).state_dir.as_deref(), Some("/tmp/state"));

        let empty = write_config(&dir, "state2.json", r#"{"stateDir":""}"#);
        assert_eq!(load_config(&empty).state_dir, None);
    }

    #[test]
    fn js_string_coercion_matches_javascript() {
        assert_eq!(js_to_string(&serde_json::json!("s")), "s");
        assert_eq!(js_to_string(&serde_json::json!(5)), "5");
        assert_eq!(js_to_string(&serde_json::json!(5.5)), "5.5");
        assert_eq!(js_to_string(&serde_json::json!(true)), "true");
        assert_eq!(js_to_string(&serde_json::json!(null)), "null");
        assert_eq!(js_to_string(&serde_json::json!([1, "a", null])), "1,a,");
        assert_eq!(
            js_to_string(&serde_json::json!({"a": 1})),
            "[object Object]"
        );
    }

    #[test]
    fn js_number_coercion_matches_javascript() {
        assert_eq!(js_to_number(&serde_json::json!(5)), Some(5.0));
        assert_eq!(js_to_number(&serde_json::json!("12")), Some(12.0));
        assert_eq!(js_to_number(&serde_json::json!(" 12 ")), Some(12.0));
        assert_eq!(js_to_number(&serde_json::json!("")), Some(0.0));
        assert_eq!(js_to_number(&serde_json::json!("abc")), None);
        assert_eq!(js_to_number(&serde_json::json!(true)), Some(1.0));
        assert_eq!(js_to_number(&serde_json::json!(false)), Some(0.0));
        assert_eq!(js_to_number(&serde_json::json!(null)), None);
        assert_eq!(js_to_number(&serde_json::json!({})), None);
    }

    #[test]
    fn boolean_field_defaults_to_true_when_absent() {
        let dir = temp_dir();
        let path = write_config(&dir, "sticky.json", r#"{"sticky":null}"#);
        assert!(load_config(&path).sticky);
    }
}
